# TASKS.md — vxedge/vxedge (core)

Ordered for the phase-1 critical path (see ../docs/TASKS.md). Each task should land with tests.

## 0. Workspace model (Track D — `docs/ARCHITECTURE.md` §6.0)

- [x] `vxedge experiment new <id> [--template minimal|slicing]` (2026-07-29) —
      `vxedge-core::scaffold`. `WorkspacePlan::plan` is pure data and unit-tested;
      `write` is the only I/O and refuses to clobber an existing directory. Ids are
      validated against OCI repository-name rules at creation, since the id becomes a
      repository path component on publish — failing there instead is the same trap a
      leading-underscore name set during the vxparts container work.
- [x] `vxedge experiment validate [path]` + `vxedge-core::workspace` (2026-07-30) — loads a
      folder by `(kind, id)` and runs every cross-file check, reporting all problems at once.
      The load-bearing ones: the cross-file **slice join** (vehicle names it, experiment
      defines it), **`carried_by` reachability** (without it per-topic-per-slice attribution
      rests on an unchecked assertion), workspace-wide component-id uniqueness (the cost of
      flat ids over structural paths), and digest-pinning for published refs only.
      Non-recursive load so `results/` is never absorbed. Tests are hand-written workspaces,
      not scaffold output, plus a generate→load→validate round-trip over every template.
- [x] `vxedge experiment lower` + `vxedge-core::lower` (2026-07-30) — §6.0 workspace → §6.1
      manifest, so a workspace runs on today's execution path without any orchestration moving
      into the CLI. Output crosses `ExperimentManifest::from_yaml`, so lowering bugs surface as
      §6.1 validation errors. Resolves what §6.0 normalises and §6.1 repeats (slice `vlan` onto
      node ifaces; traffic endpoints into `<zone-vxid>:<node-vxid>`); refuses what it cannot
      carry with the fix in the message rather than guessing.

      Building it found **six** defects nothing else had: a gateway with no part, an invented
      `traffic_profiles[].slice` field, `memory_gb` vs §6.1's `memory`, a zone/node vxid
      collision, and traffic endpoints + scrape targets in unresolvable forms. None are shape
      errors, which is why the validator missed them — this is the argument for building the
      consumer before declaring a schema done. Two became new validator checks
      (`ComponentWithoutPart`, `UnknownTrafficEndpoint`); the rest were scaffold fixes.
      Tests plan the lowered manifest through `GatewayPlan` **and** `TrafficPlan`, since §6.1's
      composition validator never inspects traffic endpoints.
### Decided 2026-07-30 — §6.0 is the only authored schema; the working directory is the workspace

Two owner decisions close the dual-schema question and the addressing question.

**One schema.** Maintaining §6.0 and §6.1 as parallel authoring surfaces was rejected: it splits
the thing an OEM engineer has to learn in two, for no gain. **§6.0 becomes the only schema
anyone writes.** §6.1 is *demoted, not deleted* — its structs stay as the internal representation
`GatewayPlan`/`TrafficPlan`/the zone backends consume, and lowering targets them. Deleting them
outright is a rewrite of the whole provisioning layer and is a phase-4 cleanup, not a blocker.
What changes now is that §6.1 stops being documented as an authoring contract and stops being
hand-written anywhere.

This is safe because **phase-1 reproducibility is anchored at the `v0.1.0` tag**, already cut
across all seven contributing repos. A frozen result reproduces at the release it was produced
at, not at HEAD — so retiring the old authoring path at HEAD costs nothing.

**The working directory is the workspace** (Terraform's model, and consistent with the
non-recursive loader). `vxedge experiment up|down|status` takes no name: it reads `.vxp` from the
current directory, and says so plainly when there isn't one. `vxedge experiment new <name>`
creates `./<name>/`.

Terraform conventions **adopted**: cwd as the unit of work; every `*.yaml` in the folder loads
together, order-independent; a `--chdir` escape hatch so scripts need not `cd`; no walking up to
parent directories (being inside `results/` must not silently resolve the parent experiment).
Terraform conventions **rejected**: a state file — manifests are the sole source of truth and
daemon state is ephemeral by design; and variables/interpolation, already rejected with HCL
because result archives are keyed to manifest SHA256.

- [x] **(1) cwd addressing** (2026-07-30) — `vxedge-core::package` owns the `.vxp` descriptor and
      discovery, separate from `Workspace::load` so a broken experiment document still yields a
      good "which workspace is this" error. Exactly one `.vxp`; the ambiguous error names the
      files, since which to remove is the engineer's call. **No walking up** — running from
      `results/` must not resolve the parent, and there is a test for exactly that. `--chdir`
      applied before dispatch so there is one notion of "here". `requires` is parsed, not
      enforced (that is task 4). `up|down|status` discover *and validate* before reporting the
      missing service, so addressing is exercisable end to end before task 2 lands.

      Fixed a pre-existing defect this surfaced: `fn main() -> Result<_, _>` formats with
      `Debug`, so every actionable error in this crate and in `vxedge-core` reached users as a
      struct dump. `main` now prints `Display` and walks the `source` chain.
- [x] **(2) `ExperimentService` in `vxedged`** (2026-07-30) — `up` provisions gateway (with the
      5G core when the manifest declares `s-nssai` slices) → zones → services. No proto change
      needed. **`up` does not start traffic**, and that is a decision, not an omission: `up` must
      be idempotent and reconciling, and traffic is a time-bounded process, not a converged
      state — "start a 60-second load run" cannot be made idempotent. It also happens to be what
      a campaign needs, since exp-001 sweeps offered load over a topology that must be identical
      each arm. The success message says so outright.

      Composition, not reimplementation: each stage submits the existing service's job and
      awaits it, so sub-jobs stay individually visible through `JobService` and a failure is
      attributable to a stage. `down` is best-effort — a teardown that stops at the first error
      strands everything after it. The manifest is resolved once and passed to every stage as
      `inline_yaml`, so an edit mid-run cannot leave the gateway on one revision and the zones
      on another.
- [x] **(3) The daemon accepts a workspace directory** (2026-07-30) — a `ManifestRef.path`
      naming a folder is loaded, validated and lowered daemon-side via
      `vxedge_core::lower::lower_path_to_yaml`. **The `experiment lower` CLI verb is deleted.**
      The reproducibility checksum now covers the *lowered* manifest, which is correct: results
      are keyed to what actually ran, and lowering is pure.
- [x] **Staged `experiment up|down`** (2026-08-01) — `ExperimentStage` GATEWAY/ZONES/SERVICES,
      repeated on both requests, empty meaning all. Additive to `vxedge.v1`, so every existing
      caller keeps its behaviour. Found by moving the campaign runners onto `experiment up`:
      all-or-nothing could serve neither exp-001 (measures on the host-side veth peers, so
      staging its zones changes the measurement path) nor exp-002 (isolation contract is one
      packaging arm at a time). Individual devices are still addressed with `zone up --vxid`,
      which is the right primitive for an arm.
- [x] **(4) `.vxp` `requires` preflight** (2026-07-31) — `gpu: none|tier2` checked at
      `experiment up`, **daemon-side** (the host that matters is the one provisioning; a
      CLI-side check asks the wrong machine the moment `--endpoint` points elsewhere), before
      anything is provisioned. exp-011 can now be authored, validated and committed while
      inert, so the unresolved GPU decision blocks nothing else.

      The probe checks what Tier 2 actually requires — compute capability **and** VRAM — not
      merely that a GPU exists. Found live: this development host has a GT 1030, so a presence
      check *passed* and would have let a Tier 2 experiment start and fail deep inside CARLA.
      Pascal at 2 GB fails on both counts, and compute capability is the binding half, since
      DeepStream/TensorRT need tensor cores and more VRAM within a pre-Turing generation does
      not help. A multi-GPU host qualifies on its most capable card.
### Decided 2026-07-30 (supersedes "demote §6.1") — reject §6.1 and model natively

The demote-not-delete compromise is withdrawn by owner decision: §6.1 is to be **rejected**, not
kept as an internal representation, and `vxedge-core` is to be refactored so the planners consume
the §6.0 model directly. Lowering then has nothing to lower to and goes away.

**What this buys, concretely: `channels` becomes executable.** §6.1 has no interaction layer, so
a workspace's messaging topology is modelled and validated today but cannot run. No amount of
lowering fixes that — it is a missing concept in the target schema, not a translation gap. Native
modelling is the only thing that delivers dimension 2, and dimension 2 is half of the vehicle
model. Second payoff: the six §6.0 defects lowering surfaced were all *translation* defects
(vlan resolution, endpoint qualification, vxid collisions, backend/image-kind). They stop
existing rather than being fixed.

**Dependency inversion, so nothing is stranded mid-refactor.** Today
`§6.0 → lower → §6.1 → planners`. Target `§6.0 → model → planners`, with §6.1 reduced to a
*temporary input adapter* that builds the same model. Frozen exp-001/002/003 keep running from
their §6.1 files throughout; the adapter is deleted last, once the equivalence runs pass and
every experiment is migrated. Deleting §6.1 before that evidence exists would remove the very
baseline the migration is being checked against.

- [ ] **(5a) `vxedge_core::model`** — the domain model as the in-code SSOT: `Experiment`,
      `Vehicle`, `Component`, `Link`, `Channel`, `Slice`, `Baseline`, `Part`. Built from
      `Workspace`; no serde-shaped types in it, so the schema and the model can differ where
      §6.0's YAML ergonomics and the planners' needs legitimately differ.
- [ ] **(5b) Port the planners onto the model**, one at a time, each keeping its tests green.
      The plan types themselves do not change, so the reconciler, backends and every integration
      test are untouched — this is a change of input type, not of provisioning. Acceptance for
      each is **plan identity** with the §6.1 path, not merely that both succeed: campaign
      scripts address `vzp0`/`vzp1` by name.
  - [x] `GatewayPlan::from_model` (2026-07-30) — both paths share one slice planner so class
        minors and defaults cannot drift. Found that the model dropped
        `network.defaults.bandwidth`: exp-001's 500 mbit bottleneck would have planned at the
        1000 mbit default, changing every number while still validating and running. A
        slice-attached device with no `backend` is now refused rather than guessed.
  - [x] `zones::plan` from the model (2026-07-30) — `zone_id` now means the *region* a device
        sits in rather than a per-device wrapper, so `zone up --vxid front-left` brings up
        everything in that region. Bundle path comes from the function's part, gated on the
        backend rather than an image-kind guess. Equivalence asserted on endpoint, peer, kind,
        vlan, backend and `runtime_name()` — the netns name the runtime actually uses.
  - [x] `TrafficPlan` from the model (2026-07-30) — §6.1's worst spelling: an endpoint is
        `<zone-vxid>:<node-vxid>` whose zone half lowering synthesized, so no author could write
        it correctly. The model resolves to a device id once, at build time. Also removed a dead
        `&ExperimentManifest` parameter from `endpoint_for` that had never been read.
  - [x] `FiveGPlan` from the model (2026-07-30) — both paths delegate to one allocator. Every
        address is derived positionally from the slice list (UE subnets, SMF/UPF, TUN devices,
        SUPIs), and a drifted SUPI surfaces as a registration timeout with nothing pointing at
        the cause, so two allocators were two chances to fail silently. **Weaker evidence than
        the other three**: exp-010 needs a provisioning-capable host, so this compares the two
        planners on a fixture rather than against a recorded result.

**5b is complete.** All four planners have a model path with an equivalence test. Next: 5c (the
§6.1 → model adapter, which is what lets the frozen experiments run through the model), then the
§6.0 schema change that finishes 5a.

- [x] **Model invariants** (2026-07-30) — `Experiment::check`. The model introduced id spaces
      §6.0 did not have (zones, devices, functions as distinct kinds), and new namespaces mean
      new collisions and new dangling references. Two checks are specific to what changed: a
      zone/device id collision (§6.0 derives a zone id from `position` and a device id from the
      component key, so a component named after its own region answers to one name as two
      things), and **a channel endpoint must be a function, not a device** — the distinction the
      model exists to make, enforced rather than documented. Device ids stay unique across the
      whole experiment because parts and traffic profiles address them flatly.

- [ ] **Positional endpoint numbering is fragile** — `vz0`/`vzp0` are assigned in traversal
      order, and campaign scripts address those names directly, so adding a device (model path:
      one whose id sorts earlier; §6.1 path: an earlier zone) silently renumbers the rest and a
      campaign measures the wrong interface. Pre-existing in both paths, not a migration
      regression. Fix is to derive the name from the device id, which is a breaking change to
      every campaign script and so wants doing once, deliberately.
- [x] **(5c) §6.1 becomes an adapter** (2026-07-30) — `ExperimentManifest::to_model()`. §6.1 is
      now an input *format* with one way in, not a planner input. Verified with the
      `adapter-diff` example over all five frozen manifests: every gateway plan, endpoint list,
      node plan and traffic profile identical.

      Found an ordering hazard that would have silently corrupted an H3 result. Endpoint names
      are positional and campaign scripts address them directly; §6.1 declares nodes as a
      sequence while §6.0 stores components in a map. exp-002 declares `cont, microvm, libvirt`
      and **sorts to** `cont, libvirt, microvm` — a sorted model would have swapped two arms, so
      `run-exp-002.sh` would have measured libvirt and reported microvm, passing every test and
      validation. Ordering now travels with the model as `device_order`, with an invariant
      asserting it lists every device exactly once.
- [ ] **(5a-schema) §6.0 declares zones/devices/functions natively** — the remaining half of 5a,
      and the **next thing to do**. Attempted 2026-07-30 and reverted: the change is right and the
      design below is settled, but it touches every fixture at once and was left half-finished
      rather than committed broken. Do it as one deliberate pass.

      Replace `composition: {zones, compute, gateway}` with three blocks, because that grouping
      calls a *place* a device:

      ```yaml
      kind: vehicle
      id: car1
      class: road            # road | air | marine | rail — extension point, nothing built on it
      subtype: car
      zones:                 # REGIONS, named not enumerated (an eVTOL has nacelles, not rear-left)
        front-left: {}
        central: {}
      devices:               # units LOCATED IN zones; role replaces the three groups
        zonefl: { role: zone-controller, zone: front-left, backend: container }
        hpc1:   { role: hpc, zone: central, backend: container, resources: { cpu: 4, memory: 8 } }
        gw1:    { role: gateway, backend: container }
        cam-fl: { role: sensor, zone: front-left, vss: Vehicle.Camera.FL }
      functions:             # units of BEHAVIOUR, deployed onto devices
        adas:    { deployed_on: zonefl, asil: B }
        percept: { deployed_on: hpc1 }
      links:                 # unchanged — the harness
        - { id: trunk-fl, endpoints: [gw1:eth0, zonefl:eth0], slice: adas }
      channels:              # endpoints are FUNCTIONS, not devices
        - { id: adas.probe, from: adas, to: [percept], carried_by: [trunk-fl, trunk-hpc] }
      ```

      Consequences that have to land in the same pass — this is what made a partial attempt
      break, and each is deliberate rather than incidental:

      - **`parts.yaml` targets function ids, not device ids.** Software runs as a function, and
        one device can host several. This is the change that ripples furthest.
      - **Channel endpoints validate against `functions`**; link endpoints against `devices`.
      - **`carried_by` reachability** walks devices, so a channel endpoint must first be resolved
        through its function's `deployed_on`.
      - **`ComponentWithoutPart` becomes per-function**, since a device without software is
        legitimate (a switch, a dumb sensor) while a function without software is not.
      - **`lower.rs` needs device → function → part**, one indirection the old schema could not
        express. It is deleted at 5f anyway, so consider deleting it in this pass instead: its
        only consumer is the daemon's workspace-directory path, which exists only because
        `vxtelemetry` still reads a §6.1 file. That makes it a cross-repo change.
      - Every hand-written test fixture and `exp-001-ws` migrate with it.

      Sequencing note: do this **before** migrating exp-002/002b/003/010, so they are migrated
      once rather than twice — the same argument that pairs exp-002's schema and vxparts cutovers.

### Blocking exp-002/002b — **closed 2026-07-31**

All three were §6.0 expressiveness gaps found by attempting the migration. exp-002 and exp-002b
are migrated (`vxperiments/experiments/exp-002-ws`, `exp-002b-ws`), both plan-identical to the
manifests they replace.

- [x] **Link endpoints take an object form carrying a node address** — `{ node, port, ip }`
      alongside the bare `node:port` string, which stays right whenever no fixed address is
      needed. exp-002's three arms share one VLAN and are told apart only by address.
      Deserialized by hand rather than with `#[serde(untagged)]`, so a misspelled key inside the
      object is reported as a misspelled key rather than as a malformed string. Addresses are
      CIDR-checked at load time.
- [x] **Where addresses belong** — on the link endpoint (the attachment), in the *vehicle*.
      A vehicle that pins none stays reusable across experiments and slice subnets. Slice
      `subnet:` as an experiment-side addressing policy is **not** implemented: no experiment
      needs auto-assignment yet, and an allocator would reintroduce order-dependence.
- [x] **`PartKind` is taken from the part, not derived from the backend** — explicit
      `kind: container | bundle` on the baseline entry, else the reference where it is
      conclusive (`local/…` is never a bundle), else the backend as a last resort. A stated kind
      is validated against the device's backend, so an H3 arm pointed at the wrong packaging is
      rejected with both halves named. A published vxparts ref is deliberately not read for
      this: its class says what kind of *node* the part loads onto, not how it is packaged.
- [x] **Device traversal order is no longer expressible, because nothing derives from it** —
      interface names come from the device id (`gateway::ifnames`, `ARCHITECTURE.md` §6.0.5).
      `Entity::device_order` and its invariant are deleted; the gateway and zone plans join by
      interface name rather than by index. Campaign scripts were updated once, deliberately.

Two more defects surfaced while migrating, both fixed:

- [x] **A VM's guest bundle was unreachable in §6.0** — the zone planner looked up a device's
      software by *function id equals device id*, true in §6.1 and false here, so every libvirt
      and qemu-microvm arm planned as a VM with no bundle to boot. Resolves through
      `deployed_on` now, with `AmbiguousBundle` when a device hosts two bootable parts.
- [x] **`zone up --vxid` could not select one arm** — a zone is a region, and exp-002's three
      arms share `front-left`, so region-only selection could start only all three at once: the
      concurrent-guest CPU contention that invalidated exp-002b's first campaign. It accepts a
      device id as well; the model already forbids a zone and a device sharing an id.

### §6.1 removal — **mostly done 2026-07-31**

- [x] `lower.rs` deleted (its only consumer, `vxtelemetry`, reads workspaces now).
- [x] `model/adapt.rs` deleted; `plan-diff` deleted with it.
- [x] The daemon holds workspace documents, not a lowered §6.1 manifest; `parse_to_model` is
      `model::load_yaml`. `experiment up` fans out over what `ZoneService` can actually plan
      rather than over §6.1 `zones[]`, one device at a time.
- [x] `ServiceDef`/`ServiceType`/`Image`/`ImageKind` moved to `vxedge-core::services`, which is
      their only consumer; they read the model's `passthrough["services"]`.
- [x] `NodePlan.backend` and traffic's `SliceMode` now use the model's own enums.
- [x] `vxedge manifest validate` validates a §6.0 workspace (directory or single file) and
      still checks §6.1/§6.2 shapes, sniffed by what the document declares.
- [x] **`manifest.rs` deleted** (2026-07-31) — `ExperimentManifest`, `VehicleManifest` and the
      shared §6.1 composition types, ~876 lines. Unblocked by migrating `vxmodel render` to emit
      §6.0 workspaces first, so no repo was ever left with a red gate: vxmodels' CI now asserts
      `kind == "workspace"` and workspace counts. `gateway::plan::resolved_backend` went with
      it — it resolved a §6.1 `image.kind` into a backend, which §6.0 states outright.
      `vxedge manifest validate` accepts §6.0 and nothing else.

### Open, found while deleting §6.1 (2026-07-31)

None block the critical path; all are consistency debts this session created or exposed.

- [x] **Two different answers to "which workspace was this"** — settled 2026-08-01, one rule in
      `ARCHITECTURE.md` §13: every file the experiment is made of, path and bytes, sorted,
      excluding `results/`. Implemented in `Workspace::content_sha256` and independently in
      `vxtelemetry`, both pinned to the same test vector. The daemon uses it for audit records;
      the archive records it as `workspace_sha256`. Original text below.

      **Two different answers to "which workspace was this".** The daemon hashes the
      *concatenated* `*.yaml` text it read (`services::resolve_manifest`); `vxtelemetry` hashes
      each file's name and bytes, `.vxp` included (`workspace_view::content_sha256`). So an
      audit record's `manifest_sha256` and its archive's `workspace_sha256` disagree for the
      same run, and neither is wrong — they are just two rules. Pick one, state it in
      `ARCHITECTURE.md` §13, and make both sides cite it. The daemon's omission of the `.vxp`
      also means an inline workspace carries no `requires`, which task 4's preflight will need.
- [x] **The `slicing` scaffold template pairs a bundle-named part with container devices** —
      fixed; the template now pairs `backend: container` devices with `kind: container` parts
      (`ghcr.io/vxedge/parts/agent/linux/app/probe`, not the bundle-named
      `.../zone/linux/stack/probe-bundle` this bullet originally flagged — confirmed by reading
      current `scaffold.rs`, 2026-08-01). This bullet was left unchecked after the fix landed —
      a stale TODO for already-done work, tracked at the top level as `docs/TASKS.md` item
      #13. Closed here rather than left to be rediscovered by the next reader.
- [x] **`vxedge manifest validate` accepts only §6.0** (2026-07-31) — the old shapes went with
      `manifest.rs`, so "workspaces only" is enforced by there being nothing else to accept.
      Its multi-document sniffing had to be fixed at the same time: it read the file with
      `from_str`, which fails outright on a `---`-separated document set, so every rendered
      workspace was silently routed into the §6.1 branch and rejected. It reads the first
      document now.

### Open, found by dogfooding (2026-07-30)

Tracked here so they are not rediscovered. None block the critical path.

- [x] `experiment new <dir> --id <experiment-id>`; `.vxp` named for the id (2026-07-30)
- [x] `car1.yaml` → `vehicle.yaml` (2026-07-30)
- [x] **Multi-document YAML in `Workspace::load`** (2026-07-31) — `---`-separated documents,
      so a whole workspace can live in one file, and `Workspace::from_yaml` gives callers with
      no directory (an inline manifest over vxAPI) the same loader. Separate files stay the
      scaffold default because they teach the model.
- [ ] **Re-derive `exp-001-ws` through `experiment new`** — it was hand-built before the scaffold
      was used for migrations. It validates and plan-diffs clean, so it is correct, but it was
      never shown to be something the tool can produce, which is the point of dogfooding.
- [x] **Positional endpoint numbering** (2026-07-31) — names derive from the device id, so
      adding or reordering a device cannot move one. Every campaign script was updated once, as
      planned. `ARCHITECTURE.md` §6.0.5 is the contract.
- [ ] **H2/`s-nssai` verification is the thinnest evidence in the port** — `FiveGPlan` and
      exp-010 are checked against fixtures, not a recorded result. Migrating exp-010 is what
      improves it.

- [ ] **(5d) Delete `lower.rs`** once no planner consumes §6.1, along with the workspace-directory
      special case in `resolve_manifest` (the daemon builds the model directly instead).
- [ ] **(5e) Execute `channels`** — the point of the exercise. Needs the messaging plane's own
      design (`docs/ARCHITECTURE.md` §7), so it lands after 5b.
- [ ] **(5f) Delete `ExperimentManifest`/`VehicleManifest` and the adapter** — last, gated on the
      exp-001/002/003 equivalence runs passing and every experiment being migrated.
- [x] **(Track D-6) Migrate `ota`/`telemetry` off `--manifest` onto workspace resolution** —
      done 2026-08-03 for `ota`: `SvcCmd::Up`/`Down`'s `--experiment` is now `Option<String>`,
      defaulting through a new `resolve_experiment_arg()` helper to
      `vxedge_core::discover(&here()?)` (the same call `experiment` verbs make) when omitted;
      an explicit path still overrides. Live-smoke-tested unprivileged (no CAP_NET_ADMIN
      needed for `ota`): cwd-resolution from inside a real workspace and the explicit-path
      override both reach the daemon correctly, and the no-workspace case fails cleanly.
      `telemetry` turned out to have nothing to migrate — every `telemetry` subcommand hits
      `scaffold_notice` regardless of args, so there was no live `--experiment`-path behavior
      to fix; that's the separate, already-tracked "daemon wiring of `TelemetryService.Collect`
      still open" gap, not reopened here. Whoever wires it should reuse
      `resolve_experiment_arg()`. `zone`/`compute`/`traffic` still address the daemon
      differently (vxid or an explicit `--manifest`) and were not re-checked in this pass — 
      don't assume either is migrated or exempt. Tracked at the top level as `docs/TASKS.md`
      item #26.
- [ ] **(7) `vehicle up` from a §6.0 vehicle document** — the phase-3 gate itself. Needs (2).
- [ ] Candidate, from Terraform's `init`: `experiment init` pre-pulls every part ref by digest
      into a local cache before `up`, so a run either has everything or fails before it starts.
      Fits the offline-first direction; not yet scheduled.
- [ ] `vxedge vehicle extract` — inline component defs to their own files, once reused
- [ ] `vxedge manifest bom` (Track D dimension 3) + `vxedge experiment publish` (refuses
      while any local ref is unpublished — a shared workspace with a local path is broken
      for the recipient by construction)
- [ ] `requires` → `resources` → host checking, once `part.yaml` carries `requires`
      (`vxparts/TASKS.md`)
- [ ] Publish the manifest JSON Schema generated from `vxedge-core` as a config part —
      editor autocomplete and inline validation while hand-authoring

## 1. Scaffolding & contract (phase 1)

- [x] Cargo workspace: `vxedge-api` + `vxedge-core` + `vxedged` + `vxedge-cli` crates; CI
      (build/clippy/fmt/test + buf lint)
- [x] `proto/vxedge/v1/`: Gateway/Zone/Compute/Svc/Vehicle/Experiment/Traffic services +
      JobService (Get/Watch) + EventService (Subscribe) + TelemetryService (StreamMetrics,
      Collect); buf config; Rust codegen via vxedge-api build.rs
- [x] `vxedge-core`: experiment-manifest structs (§6.1 incl. slices/slice_policy/backend/
      realtime/carla/gpu/`telemetry.prometheus.optional_scrape_targets`) + serde_yaml
      parsing + semantic validation (slice refs, vxid uniqueness, slice-mode consistency,
      service refs) — vehicle manifest structs delivered below (2026-07-19)
- [x] `vxedge-core`: vehicle-manifest structs (§6.2) + validation (2026-07-19).
      `VehicleManifest` (`vehicle`/`network`/`services`/`compute`/`zones`) reuses the same
      `Zone`/`ComputeNode`/`Network`/`ServiceDef` types an experiment manifest carries;
      `Vehicle` adds `signal_plane.kuksa` (phase 3), `comms.ros2_rmw`, and
      `world.simulator`/`world.image` per §6.2 exactly. Semantic validation (vxid
      uniqueness, slice refs, slice-mode consistency, service refs, the
      `slice_policy` guard above) is shared with `ExperimentManifest` via a new
      `validate_composition()` free function rather than duplicated — both manifest
      kinds must be internally consistent the same way. `vxedge manifest validate`
      now sniffs the top-level key (`vehicle:` vs `experiment:`) and validates
      accordingly, so it stays one command for both — unblocks M2's `vxmodel render`
      pipeline (`vxmodels/TASKS.md`), which needs to run `vxedge manifest validate
      --json` against a rendered vehicle manifest. 4 new vxedge-core tests + CLI
      live-verified both directions (vehicle manifest OK + JSON, experiment manifest
      still OK); 70 unit tests, clippy/fmt clean.
- [x] `vxedged`: job engine (async tasks + watch-channel progress), event bus
      (EventService.Subscribe live), structured JSONL audit log of mutating calls; Gateway
      Apply/Up/Down submit real jobs (validate + plan; Up/Down fail honestly at the
      provisioning step until §2 lands)
- [ ] `vxedge-cli`: auto-spawn ephemeral daemon *(done: full clap tree, `--endpoint`, live
      `status`, and `up`/`down` submitting jobs with streamed progress + JSON output)*
- [x] `vxedge manifest validate` subcommand (2026-07-13): local-only (no daemon connection
      — same read-only check `vxedge up` runs before provisioning, exposed standalone),
      wraps `vxedge-core::ExperimentManifest::from_yaml` directly; `--json` for CI.
      Ready to power manifest-validation CI in vxperiments.
- [x] `vxedge doctor` (2026-07-13): host preflight per docs/TESTBED-SETUP.md tiers 0-3
      (kernel/Rust/protoc/buf/iproute2; CAP_NET_ADMIN/nftables/PREEMPT_RT; KVM/QEMU/
      libvirt/OVS/WireGuard/GPU/container-runtime; phase-3 stub), `--tier N` to scope
      what's required vs. informational, actionable fix hints, `--json` for CI

## 2. vxgateway backend (phase 1 — the research-critical piece)

H1 minimum (exp-001, single host): bridge + VLANs, tc HTB, plan/observe/verify, netns tests
— **done 2026-07-10**. nftables/WireGuard/VXLAN/FRR trail within the phase (not needed for
exp-001 measurements).

- [x] Linux-bridge provisioning (kernel VLAN filtering; OVS deferred): trunk uplink +
      per-slice VLANs + access-port endpoints from `network.slices`/zones, via
      `gateway::{plan,netops,provision}` — iproute2 argv exec with `-json` observation
- [x] tc HTB queue tree per slice (min_rate/max_rate/priority floors, flower vlan_id
      filters → classes) — H1 mechanism
- [x] `GatewayService.Apply/Up/Down/Status` wired: Apply = plan-only; Up = provision +
      observe + verify (fails if observed ≠ planned); Status = live observation per slice;
      clear CAP_NET_ADMIN error when unprivileged
- [x] netns integration test (CI job `netns-integration`, sudo on ubuntu-latest):
      apply → observe → double-apply idempotency → teardown → double-teardown
- [ ] nftables per-slice firewall; WireGuard link setup; VXLAN inter-host links
- [ ] FRR config rendering (BGP optional path)
- [ ] Port NetOps from iproute2 argv exec to pure rtnetlink/netlink (recorded deviation
      from AGENTS.md; plan/observe layers are unaffected)
- [ ] Reconciler: converge partial state (up-after-partial-failure repairs deltas rather
      than failing verify)

## 3. Zone & compute backends (phase 1 minimal, phase 2 full)

- [x] Zone backends (2026-07-11): container (netns + veth→eth0 + manifest IP),
      qemu-microvm (raw QEMU direct kernel boot, pidfile, taskset pinning), libvirt
      (virsh qemu:///system, precreated tap managed=no, vcpupin) — idempotent, observed
      status; endpoint kinds veth|tap in the gateway plan; per-iface `ip:` in the schema
- [x] `ZoneService.Up/Down/List` live (jobs with per-node progress + post-verify);
      CLI `zone up/down/list`; container cycle covered by the netns CI test
- [ ] libvirt: storage pools with SHA256-digest disk images + hugepages (beyond direct
      kernel boot); QMP instead of pidfile for microvm control
- [ ] container backend: OCI runtime integration (payload currently exec'd by the runner)
- [x] `ComputeService.Up/Down/List` live — **delivered 2026-08-01**, CLI `compute
      up/down/list`. The `service-boundaries` skill's own test decided the shape: a
      second *backend* would need a second implementation, and none exists — an
      hpc-role device already ran under the exact same container/qemu-microvm/libvirt
      code as any other, since `plan_nodes_from_model` never filtered by role. So
      `ComputeService` is not a new backend, it is a second RPC surface over the same
      `NodePlan`/`zones::node_up`/`node_down`/`node_status`, giving compute nodes their
      own vocabulary and their own `compute.up`/`compute.down` events without adding a
      variation point that doesn't exist. `NodePlan` gained a `role: DeviceRole` field
      (populated straight from the device declaration, never guessed) so
      `ComputeSvc::plans_for` can filter to `role == Hpc` — the one genuinely new piece
      of information the split needed. `ZoneSvc` itself is untouched (copied rather than
      shared, since it is load-bearing for H1/H3/H4 and a shared helper would put a
      second caller's requirements on code those frozen results depend on).
      **Deliberately not wired into `ExperimentService`'s zones stage**: that stage
      already brings up every device regardless of role, so wiring compute in as a
      second stage would double-provision hpc-role devices without first carving them
      out of zones — a real, separate decision (`VehicleService`'s planned
      gateway→zones→compute→signal-plane→services order is where it belongs) left open
      rather than guessed at. Live-verified against a real daemon: `compute up`/`list`
      correctly refuse before any experiment is active, and correctly reach the role
      filter once one is (`ecu1` refused as not compute, `hpc1` accepted) — full
      provisioning is the same code the netns test already proves, so it was not
      re-proven under the new name. `Apply`/`Remove`/`PushImage` are `unimplemented`
      stubs, matching `ZoneService`'s own existing precedent for the same three RPCs.
      139 tests total (was 138), clippy `-D warnings` and fmt clean.

## 3b. Service lifecycle (ota, telemetry — §8)

- [x] `SvcService.Up/Down/Status` live (2026-07-12): generic `vxedge-core::services`
      process backend spawns the service binary as a host-local child, pidfile-tracked
      like the qemu-microvm zone backend; `ota` is implemented (spawns `vxota serve` per
      the manifest's `services.<key>.config.{artifacts,listen}`), `telemetry` returns a
      clear "unsupported by this backend yet" error (it's driven directly via the
      `vxtelemetry` CLI by experiment runners today, not through vxedged). CLI `ota
      up/down` wired end-to-end; smoke-tested live (spawn, reach over HTTP, idempotent
      re-up, teardown, zero zombies).
  - Found and fixed a real bug during that smoke test: the spawned child was never
    `wait()`-ed, so after `kill` it became a zombie — `/proc/<pid>` and `kill -0` kept
    reporting it as alive forever, both to `svc_down`'s own liveness check and to anyone
    else polling status. Fixed with a dedicated reaper thread per spawn.
- [ ] `vxcompute` scheduling: v1 co-locates every service with vxedged on the host — real
      placement onto a compute node is blocked on the `vxcompute` backend not existing yet
- [ ] WireGuard `ota`/telemetry slice transport (§7) — v1's `config.listen` must be an
      address zone nodes can already reach (typically the gateway's own bridge address)
      since there's no encrypted overlay yet
- [x] `telemetry` service type in the generic process backend (2026-07-31) — landed with the
      `ServiceBackend` seam: a backend translates a `ServiceDef` into a command line, and the
      supervision every service shares (idempotent up, pidfile, child reaping, graceful-then-
      forceful down) stays in one place. Telemetry spawns `vxtelemetry ingest --workspace <ws>`
      and reads its scrape targets from the workspace rather than restating them in
      `services.telemetry.config`, which would be a second place to edit. `--vxtelemetry-bin`
      joins `--vxota-bin` on vxedged, both behind a `ServiceBinaries` struct so the next service
      type adds a field rather than a parameter to every caller.

## 4. Phase 2

- [ ] Open5GS/UERANSIM driver: `mode: s-nssai` slice provisioning (H2). *In progress,
      major milestone reached 2026-07-14 — **end-to-end slice-differentiated PDU
      sessions proven live**:
      - `vxedge-core/src/fiveg/`: `FiveGPlan` (pure-data, mirrors GatewayPlan) compiles
        `mode: s-nssai` slices into the full 8-NF Open5GS core (NRF/AMF/AUSF/UDM/UDR/
        PCF/BSF/NSSF + per-slice SMF/UPF) plus UERANSIM gNB/UE configs — deterministic
        loopback addressing, validated sst/sd/slice-id, typed errors, 15 rendered files,
        10 unit tests. All config-schema fixes (t3512, nssf s_nssai mapping, smf dnn
        nesting, per-slice tun devices) found and fixed against live Open5GS 2.8.0.
      - **Containerized** (hybrid policy, ARCHITECTURE §15): `containers/fiveg/` —
        vx5g-core (Open5GS 2.8 + mongosh + open5gs-dbctl pinned to v2.8.0) and vx5g-ran
        (UERANSIM v3.2.7) images, rootless podman, digest-pinned; `pod-up.sh` brings up
        the full core in dependency order and inserts subscribers by parsing rendered
        `ue-*.yaml` files directly (rendered config stays the single source of truth).
      - **Live E2E proof**: gNB completes NG Setup against the rendered AMF; both test
        UEs complete Initial Registration and PDU Session Establishment **each on its
        own S-NSSAI**, landing on their slice's subnet (adas → 10.46.0.2/uesimtun0,
        infotainment → 10.47.0.2/uesimtun1). This is the first confirmation the
        rendered stack forms real, slice-differentiated sessions, not just "daemons
        start clean".
      - Known gap at the time, tracked: dbctl's slice helper needs an SD — a slice
        declared without one fell back to the default subscribed slice (`pod-up.sh`
        comment). **Closed 2026-07-17**, see below.
      - **Rust-native pod lifecycle delivered 2026-07-17**
        (`vxedge-core::fiveg::lifecycle`): ports `pod-up.sh`'s proven sequence into a
        tested module — `core_run_specs`/`subscriber_commands` are pure data (8 unit
        tests: phase ordering, per-container flags, sd-present/absent branching),
        `fiveg_up`/`fiveg_down`/`fiveg_status` are idempotent (`podman pod
        exists`/`rm --ignore`, status via `podman ps --format json`, field names
        checked against the live binary rather than assumed). Proven live twice: the
        `#[ignore]`d integration test (`fiveg_podman.rs`) passed on the first run —
        pod up, all 13 containers running, idempotent up/down, teardown — and a
        manual spot-check against a real gNB + both UEs reproduced the exact same
        result as the shell version (same subnets, same NG Setup, same PDU session
        establishment). **This module is the mechanism only** — not yet wired to any
        vxAPI RPC or manifest verb.
      - **Explicit subscriber documents delivered 2026-07-17**
        (`vxedge-core::fiveg::subscriber`): drops `dbctl` entirely — writes the
        MongoDB `subscribers` document directly (`mongosh --eval`), sourced straight
        from `FiveGPlan`, always with the slice's true sst and its sd only when
        declared. Confirmed the SD gap was a real bug, not just a CLI inconvenience:
        `dbctl`'s SD-less fallback hardcoded `sst: 1` regardless of the manifest.
        **Live-caught a second, worse bug during verification**: the first cut wrote
        `session` as a bare object instead of the array Open5GS's schema requires — a
        find/replace placeholder ate its own brackets. UDR accepted the malformed
        insert silently but failed converting it back to JSON on query, breaking PDU
        session establishment for **every** subscriber (not just SD-less ones) with
        `DNN_NOT_SUPPORTED_OR_NOT_SUBSCRIBED`. Root-caused by isolating a single-slice
        pod and diffing against a known-good `dbctl`-inserted document field by field;
        fixed by building the nesting directly instead of string-replace; a named
        regression test now guards the exact shape. **Re-verified live three times
        after the fix**: single-slice PDU session established; two-slice pod (one SD,
        one without) — both PDU sessions established, both UEs on their correct
        distinct subnets, matching every prior proof; MongoDB document inspected
        directly to confirm correct sst/absent-not-blank-sd/array shape.
      - **RAN split from CN into a separate pod, joined by a real transit link —
        delivered 2026-07-17.** Investigating uesimtun↔vxgateway bridging found the
        single-pod topology couldn't support it: isolating a single-slice pod showed
        the UE's `uesimtun<i>` and the UPF's `vxtun<i>` installing *identical*
        connected routes for the same subnet in one routing table — an ambiguity a
        real deployment never has (the UE is a physically separate device). Also
        found live: Open5GS's UPF creates its tun device but leaves IP configuration
        to the deployer (standard bare-metal practice) — it came up `DOWN`, unrouted,
        nothing else configuring it. Fixed: `fiveg_up` now creates a CN pod and a RAN
        pod, joins them with a veth pair on a new transit subnet (`10.99.0.0/24` —
        AMF's NGAP and each UPF's GTP-U address moved there; CN-internal SBI/PFCP
        traffic stays loopback, unchanged), starts gNB in the RAN pod, and
        explicitly configures each UPF's tun device. Two more real bugs live-caught
        during verification, both fixed: (a) running the test under `sudo` switched
        `podman` to root's own separate rootless identity/storage — fixed by scoping
        `sudo` to only the `ip`/`nsenter` calls that need CAP_NET_ADMIN, keeping
        `podman` itself unprivileged; (b) phase-ordering, found twice — UPF's and
        then AMF's daemons tried to bind their new transit-subnet addresses before
        the transit link existed, both binds failed and both processes exited; fixed
        by starting AMF/SMF/UPF only after the transit link and each UPF's GTP-U
        address are in place. **Reverified live after every fix**: the integration
        test passed clean (CN 13/13, RAN's gNB running, idempotent both ways,
        teardown), and a manual spot-check registered both UEs against the real
        split topology — NG Setup and both PDU sessions succeed, both land on their
        correct subnets, matching every prior proof — but this time NGAP/GTP-U
        genuinely crossed a real inter-pod link, not a shared netns, which is
        stronger, more representative proof than before.
      - **`GatewayPlan` stops rejecting s-nssai slices, delivered 2026-07-17**
        (`vxedge-core::gateway::plan`): the reconciler that produces H1/H3/H4's
        frozen numbers previously hard-errored (`SnssaiNotSupported`) the moment a
        manifest declared any `mode: s-nssai` slice, forcing mixed vlan+s-nssai
        manifests to be split by hand. Fixed additively and with zero touch to the
        vlan/HTB provisioning path itself: `GatewayPlan` now carries a
        `snssai_slice_ids: Vec<String>` field — tracked, summarized, explicitly
        **not** provisioned by this reconciler (fiveg owns that lifecycle) — and
        only rejects a manifest when it has *no* slices of either kind
        (`PlanError::NoSlices`, renamed from `NoVlanSlices`). Re-ran `manifest
        validate` against all four existing experiment manifests (001/002/002b/003)
        after the change: all still `OK`, confirming no regression to the
        vlan-only path H1/H3/H4 depend on.
      - **`bridge_up`/`bridge_down` delivered 2026-07-17**
        (`vxedge-core::fiveg::lifecycle`): extends the CN pod with a second veth,
        the host side plugged into a Linux bridge (`master <bridge>`) and the pod
        side given `BRIDGE_ATTACH_CN_ADDR` (`10.98.0.1/24`, a subnet distinct from
        the CN↔RAN transit subnet) with `ip_forward` turned on in the CN netns so
        traffic can cross from the bridge into the UPF's GTP-U tunnel. Deliberately
        does not create the bridge itself or touch `vxbr0`/VLAN membership — that
        stays the caller's decision, exercised via a new `bridge_up_debug` example
        against a throwaway test bridge, not the real gateway bridge.
      - **uesimtun↔vxgateway bridging mechanism proven live, 2026-07-17**: stood up
        the full CN+RAN split, attached the CN pod to a real Linux bridge
        (`vx5g-testbr`) via `bridge_up`, added host routes for both slice subnets
        via the bridge, and pinged both UEs from the host through the bridge and
        UPF GTP-U tunnel. **adas (10.46.0.2, uesimtun0): 3/3 packets, 0% loss,
        sub-ms RTT** — the last-mile gate is genuinely crossed: host traffic rides
        a real bridge into a real PDU session and back. **infotainment
        (10.47.0.2, uesimtun1): 3/3 sent, 0 received, 100% loss** — root-caused,
        not left as a mystery: `ip rule show` in the RAN pod showed both UEs'
        per-source policy rules (`from 10.46.0.2`, `from 10.47.0.2`) pointing at
        the *same* routing table, `1000` — UERANSIM hardcodes that table ID per UE
        rather than deriving it from the interface index, so the second UE's
        `ip route add default ... table 1000` collided with the first's
        (`RTNETLINK answers: File exists`, previously seen as a benign log line in
        every earlier single-flow test today). This is UERANSIM's actual deployment
        assumption — one UE per network namespace — surfacing exactly the same
        class of co-location bug the CN/RAN split fixed one layer up, now one layer
        down at UE granularity. **Conclusion**: the bridging *mechanism* is proven
        for one slice; **simultaneous multi-slice traffic needs one RAN netns/pod
        per UE**, tracked as a new, specific follow-up below — not a blocker for
        this milestone, which was the bridging mechanism itself.
      - **GatewayService wiring for s-nssai slices, delivered 2026-07-17**:
        `vxedge up` now provisions the fiveg CN/RAN pods and attaches the CN
        pod to the real `vxbr0` (a new reserved `FIVEG_UPLINK_VLAN` for the
        pvid-untagged port — s-nssai slices carry no manifest `vlan:` of their
        own to reuse) whenever the manifest declares any s-nssai slice, in the
        same `Up` Job as the existing vlan/HTB provisioning; `down`/`status`
        are symmetric. **Three real bugs found live wiring this end to end,
        all fixed**: (a) `setcap cap_net_admin+ep` on `vxedged` does *not*
        grant CAP_NET_ADMIN to the `ip`/`bridge`/`tc` child processes
        `gateway::netops::Shell` shells out to — capabilities don't propagate
        across `fork`+`exec` without ambient-set handling this shell-out code
        doesn't do, so the documented "setcap the binary" deployment advice
        was actually wrong and untested; fixed by extending the `sudo`-wrapping
        pattern already proven live in `fiveg::lifecycle` to `Shell` itself
        (new `escalate` flag, default `false` — the existing privileged
        `gateway_netns.rs` test, which already runs its whole process under
        `sudo`, is untouched byte for byte); (b) `status` only merged fiveg's
        health into the top-level detail string when degraded, so a fully
        healthy 5G core showed no evidence it had even been checked — fixed
        to always append it, and to label vlan vs s-nssai slice counts
        separately (they were conflated under one ambiguous "N slice(s)");
        (c) the CLI's `JsonStatus` never included per-slice detail at all (a
        pre-existing gap predating this change, applying equally to vlan
        slices) — added `slices` to both plain-text and `--json` status
        output, the only way to actually observe the new `SliceStatus`
        entries live. **Live-verified end to end** against a real `vxedged` +
        podman + `vxbr0`: CN pod (13 NF containers) and RAN pod (gNB) come up
        through the real RPC path, `vx5g-br0` lands as VLAN 4000
        pvid-untagged on `vxbr0`, `status` reports both `adas` and
        `infotainment` slices up with live container counts, `down` tears
        everything down cleanly (pods and `vxbr0` both gone). No regression:
        all 44 unit tests plus clippy/fmt stayed clean throughout.
      - **One RAN pod/netns per UE, delivered and live-verified
        (2026-07-17)**: fixes the UERANSIM routing-table-1000 collision
        found in the bridging proof — UERANSIM hardcodes that table id per
        UE rather than deriving it from the interface, so co-located UEs
        collided on it. Fix mirrors the CN/RAN split one layer down: each UE
        now gets its **own pod** (hence its own private netns, hence its own
        private table 1000 — no shared kernel state left to collide on),
        joined to gNB through a new bridge *internal to the RAN pod's own
        netns* (`ran_bridge_name`) rather than a shared namespace;
        `RAN_TRANSIT_ADDR` moved from the transit veth onto that bridge so
        it stays reachable from both the CN pod and an arbitrary number of
        UE pods. New `ue_up`/`ue_down` primitives (not folded into
        `fiveg_up`, matching the existing vxprobe/vxtraffic provisioning-vs-
        traffic split) plus a new `SliceSession.ue_ran_link_addr` field
        (`10.99.0.<50+i>`, regression-tested to never collide with
        `upf_gtpu_addr`'s range even at the max slice count). **Live-verified
        in two parts**: the extended `fiveg_podman` integration test (now
        exercising `ue_up`/`ue_down`, idempotency, teardown) passed clean
        against real podman; a manual run then inspected both UEs' routing
        tables directly — neither shows the earlier "TUN configuration
        failure" log line, and `ip route show table 1000` returns a real
        default route for each, confirmed by inspection, not just absence of
        an error. **Re-ran the exact bridging ping test that originally
        found the bug**: adas still 3/3 0% loss; infotainment now *also*
        3/3 0% loss (was 100% loss before this fix). This was the last item
        blocking H2 numbers.
      Remaining: image-digest recording into results metadata (not H2-
      blocking — a reproducibility/provenance nicety for the eventual
      results archive).*
- [ ] **H2 campaign (exp-010), in progress since 2026-07-17.** Started once
      the mechanism (above) closed the last blocker. Staged plan: model H2
      in SysML v2 first (M3, `vxmodels`) → hand-derive `exp-010.yaml` →
      real `TrafficService`/`ExperimentService` RPC surface (no bash-runner
      hardcoding, matching H1's own gap) → run the campaign → compare
      against exp-001's frozen numbers. Progress so far:
      - **Model authored, model-first, gate 1 real** —
        `vxmodels/models/experiments/exp-010-h2-real-slicing.sysml`:
        `Exp010RealSlicingComparison`, reusing exp-001's own
        `CampaignFrame`/`InterferenceSource` unchanged plus a new
        `SliceMode` (vlan/snssai) variant dimension, so the vlan arm is
        directly comparable to exp-001's frozen archive. Parses/type-checks
        clean against the pinned SysIDE toolchain (digest-verified before
        use), no regression to the existing models. Honest gap recorded in
        the model itself: `vxmodel` (M2) doesn't exist yet, so
        `exp-010.yaml` needs hand-derivation, not automated rendering, until
        it lands — gate 2 of the double gate runs partially (real `vxedge
        manifest validate --json`, no render step) for now.
      - **The probe data path, delivered and live-verified same day**: H1's
        methodology runs `vxprobe reflect` *on* the device under test, not
        pinged from outside — so H2 needs a reflector running inside each
        UE's own pod, reachable through its actual PDU session. `vxprobe`
        (`vxagents`) is now baked into the `vx5g-ran` image (staged via a
        small script, `containers/fiveg/build-vx5g-ran.sh` — `vxagents` is
        a separate repo, unreachable from this build context directly,
        the standard cross-repo-binary staging pattern) and a new
        `fiveg::lifecycle::ue_probe_up` starts it, joined to the UE's pod.
        **Live-caught a real bug, not a mechanism gap**: the first cut bound
        the reflector to `0.0.0.0`. `vxprobe send` from the host reported
        100% loss for both slices — but `uesimtun0`'s own RX packet counter
        proved every probe genuinely arrived (RX +20 for 20 sent); the
        failure was entirely on the *reply* side (TX +1). Root-caused, not
        patched around: an unbound-source socket's reply route resolves
        before the UE's own per-source policy rule
        (`from <addr> lookup rt_uesimtun<i>`) can pin it to the tunnel, so
        replies leaked out via UERANSIM's own unrelated `tap0` default
        route instead. Fixed by waiting for the PDU-session address to
        exist (`wait_for_ue_pdu_addr`, a bounded poll — not a fixed sleep,
        since binding before registration completes would just fail
        outright) and binding the reflector explicitly to it. **Live-
        verified after the fix**: real `vxprobe send`/`reflect` round-trips
        (not ping) through the actual bridge + PDU session, both slices
        simultaneously: adas 300/300 received, 0% loss, p50 428 µs / p99
        583 µs; infotainment 300/300 received, 0% loss, p50 488 µs / p99
        685 µs. All 48 unit tests plus clippy/fmt stayed clean throughout.
      - **`TrafficService` delivered and live-verified end to end, same
        week**: new `vxedge-core::traffic` module (`TrafficPlan` — pure,
        resolves `traffic_profiles.source/target` for both slice modes
        against the *already-computed* `GatewayPlan.endpoints`, never
        re-deriving peer-interface naming independently, per
        `zone-backend-contract`'s join invariant; `traffic::lifecycle` —
        real `vxprobe`/`vxtraffic` host processes, pidfile-tracked exactly
        like `services.rs`'s own process backend, reusing
        `fiveg::ue_probe_up`/`wait_for_ue_pdu_addr` outright for s-nssai
        targets rather than reimplementing UE handling a second way).
        `TrafficSvc` wired into `vxedged` (`vxapi-protobuf-design`
        conventions: `Job`-returning `Up`/`Down`, dotted `"traffic.up"`
        kind — the proto already had the right shape, no changes needed)
        and the CLI (`vxedge traffic up/down --profile <id>`), replacing
        the scaffold. `GatewayService.Up` also now brings up every s-nssai
        session's **UE pod** (a real gap found before any live test:
        nothing called `fiveg::ue_up` anywhere, so `ue_probe_up` would
        have failed immediately with no pod to run inside); `Down` tears
        every UE pod down first. **Three more real bugs live-caught and
        fixed getting this to actually pass through the real production
        `vxbr0`** (not a throwaway test bridge): (1) two real node
        references on the same VLAN would have silently collided on the
        identical `10.<vlan>.0.2` address — caught by the module's own
        test suite before touching any I/O, now rejected explicitly
        (`BothEndpointsRealOnSameVlan`); (2) a misconfigured s-nssai probe
        profile resolved its target's real transport address *before*
        checking whether a session even existed for it, so it failed on a
        live podman call instead of a cheap validation error — reordered so
        `ensure_reflector` validates first; (3) `apply_fiveg_uplink` gave
        the CN pod's port correct VLAN membership, but nothing gave the
        **host itself** a reachable address or route into that VLAN at all
        — fixed with a `<bridge>.<FIVEG_UPLINK_VLAN>` host subinterface
        (`self` VLAN membership on the bridge device, the standard
        Linux-bridge pattern) plus one route per UE subnet
        (`apply_fiveg_ue_routes`), replacing what every earlier manual
        verification script added by hand with real, tested code. Also
        closed a portability gap found while auditing for hardcoded
        values: nothing stopped a manifest from declaring `vlan: 4000`,
        silently colliding with the reserved uplink VLAN — now rejected
        (`VlanCollidesWithFiveGUplink`). **Live-verified end to end**: a
        real manifest with two vlan slices + one s-nssai slice, all three
        `traffic_profiles` running *simultaneously* through the actual
        production path — `adas-probe` (vlan) 2000/2000 received, 0% loss,
        p50 44 µs; `infotainment-load` (vlan) 99.98 mbit achieved against a
        100 mbit target; `h2-probe` (s-nssai, **UE as the reflector**,
        through the real `vxbr0`) 2000/2000 received, 0% loss, p50 347 µs
        — then clean teardown, host confirmed empty. 63 unit tests total,
        zero regressions, clippy/fmt clean throughout. This exact scenario
        is now a permanent, `#[ignore]`-gated integration test any joiner
        can run (`crates/vxedge-core/tests/traffic_podman.rs`, mirroring
        `fiveg_podman.rs`'s own pattern) — the manual verification scripts
        that found these bugs lived only in an ephemeral job scratch dir
        and would otherwise have left no reproducible trace in the repo.
      Remaining: hand-derive `exp-010.yaml`, actually run the campaign
      (repetitions × load levels × 2 arms) and compare against exp-001's
      frozen numbers — the mechanism itself has no known gaps left.
- [x] **Image-digest recording into results metadata — delivered in
      `vxtelemetry`, not here (2026-07-18).** The investigation this item
      called for ("find whichever repo actually owns results-archive
      writing today") answered: `vxtelemetry/src/archive.rs` writes every
      campaign zip's `metadata.json` — nothing in `vxedge-core` was the
      right hook point. `vxtelemetry collect` gained a repeatable
      `--record-image <ref>` flag (caller-driven on purpose: which images
      matter is the campaign runner's knowledge; vxtelemetry guessing would
      hardcode this repo's fiveg image naming into that one), resolving via
      `podman image inspect` with repo-digest-preferred/image-ID-fallback
      and honest `unresolved: …` values on failure. Live-verified against
      real podman; wired into `run-exp-010.sh` for the fiveg image set.
      Full account: `vxtelemetry/TASKS.md`.
- [x] **Per-slice downlink QoS enforcement for s-nssai slices, delivered and
      live-verified (2026-07-18).** Found via exp-010's first real campaign
      (H2's first live numbers, 5 reps x 5 loads, 60s window): the snssai
      arm's adas p99 grew 1.25x baseline->600mbit while the vlan arm's
      (exp-001, same-day comparison) stayed flat at 0.93x — H2 did not hold
      as measured. Root-caused by code inspection, not left as a data
      mystery: `fiveg::subscriber` hardcodes an identical QCI/5QI=9, ARP
      priority_level=8, and 1 Gbps AMBR for *every* subscriber/slice;
      `render_smf`/`render_upf` never reference the manifest's `Slice.qos`
      block at all.
      **The obvious-looking fix (render `Slice.qos` into subscriber
      5QI/ARP/AMBR) was checked empirically before writing any code, not
      assumed** — a subscriber's session AMBR was hand-patched to 20mbit
      live via `mongosh` and the UE's own `uesimtun0` re-measured under a
      600mbit offered load: received traffic stayed at the full,
      unconstrained rate regardless of the AMBR value. Open5GS's software
      UPF in this deployment records subscriber AMBR but does not enforce
      it — the originally-planned fix would have been standards-correct
      and functionally inert.
      **Real fix**: `gateway::provision::apply_fiveg_slice_qos` — the exact
      same proven HTB primitives `apply_slices` already uses for the vlan
      arm (root class + per-slice rate/ceil/priority classes), applied to
      the host-side `apply_fiveg_uplink` interface (the real convergence
      point for both `adas-probe`'s and `infotainment-load`'s
      host-originated downlink traffic — the direct s-nssai analog of
      `vxup0`), classified by destination UE subnet (`tc ... u32 match ip
      dst <ue_subnet>`) instead of VLAN tag. `fiveg::SliceSession` gained
      an opt-in `qos: Option<SliceSessionQos>` (only present when the
      manifest declares `Slice.qos` — an unconditional default, the way
      `SlicePlan` defaults an *absent* vlan `qos`, would have silently
      capped every s-nssai manifest that predates this field at 1mbit). No
      teardown code needed: `teardown_fiveg_uplink` deleting the host
      interface already frees the whole attached qdisc/class/filter tree.
      `exp-010.yaml` now declares `qos` on both slices (copied verbatim
      from `exp-001.yaml`: adas 100/200/1, infotainment 0/400/5) to opt in.
      65 unit tests (2 new), clippy/fmt clean.
      **Live-verified end to end**: `tc class show`/`tc filter show`
      confirmed the tree installs exactly as designed (`1:20` adas
      rate=100/ceil=200/prio=1, `1:21` infotainment rate=1/ceil=400/prio=5,
      filters matching `10.46.0.0/16`/`10.47.0.0/16` respectively). Sending
      600mbit at infotainment while adas-probe ran simultaneously: the
      sender's own reported rate dropped to 386.5mbit (kernel backpressure,
      not application-level throttling), the UE's own `uesimtun0` received
      ~395.5mbit over its true elapsed window — both landing right at the
      declared 400mbit ceiling, zero packet loss (paced, not dropped) — a
      dramatic, unambiguous contrast with the AMBR test's zero effect.
      adas-probe held p50 276.5µs/p99 340.5µs throughout, comparable to the
      pre-fix unloaded baseline (329.2µs) rather than the 410.9µs it grew
      to under load before.
      **Confirmed by the real campaign, same day: H2 HOLDS** (adas p99
      growth baseline→600mbit 1.00x snssai vs 0.93x vlan). The first 5×5
      rerun with this mechanism was still formally borderline on an
      unmitigated host (`vxperiments/scripts/lib/host_pinning.sh` closed
      that separately — cpu governor, cpu affinity, randomized load order
      — see `vxperiments/TASKS.md` for the full account); this fix's own
      correctness was never in question once live-verified, and the final
      campaign confirms it delivers the isolation it was built for.
      Distinct from the URSP-style `slice_policy` item just below (that
      one's about a UE carrying multiple concurrent sessions with
      client-side traffic-class routing; this one's about the network side
      actually treating slices differently under contention, now
      delivered and confirmed). Full account: `docs/STATUS.md`,
      `vxmodels/models/requirements/verification.sysml`'s
      `Exp010RealSlicingVerification` (`campaign` → `htbFixRetest` →
      `finalRetest`).
- [ ] **URSP-style `slice_policy`.** Sized 2026-07-17, design pass done
      2026-07-19, deliberately not built: not a small add — the manifest
      schema (ARCHITECTURE.md §6.1) is `match: <traffic class|dscp|app id>
      → slice: <slice-id>` **per zone node**, meaning a single real compute
      node needs *multiple concurrent PDU sessions* (one per slice) with
      traffic-class-based policy routing selecting between them. But
      `FiveGPlan`'s current model is 1 UE = 1 `SliceSession` = 1 slice — no
      UE has more than one PDU session. Supporting URSP means: (1) reworking
      `FiveGPlan`/`SliceSession` to let one UE carry several concurrent
      sessions across different S-NSSAIs, (2) rendering multiple
      `sessions:` entries into a single `ue-*.yaml` (UERANSIM supports
      this; not yet exercised here), (3) real DSCP/`tc` policy routing on
      the node side selecting which `uesimtun<i>` a given traffic class
      rides. Weighed against building it now and decided against: not
      H2-blocking, reworks `FiveGPlan` (load-bearing for H2's now-verified
      HOLDS result), and depends on unexercised UERANSIM multi-session
      routing-table behavior in the exact area (per-UE routing table 1000)
      that already caused a real bug earlier in this program. Nothing in
      remaining phase-2/M2/M3 work needs it. Left genuinely open until a
      concrete OEM use case needs multi-session UE-side routing.
      **Guard delivered instead (2026-07-19)**: the manifest schema already
      accepted `slice_policy` and validated its `slice:` reference while
      execution silently ignored it — a real footgun (a declared policy
      with zero effect, no warning). `ExperimentManifest::validate()` now
      rejects any node with a non-empty `slice_policy` via
      `ValidationError::SlicePolicyNotImplemented`, so `vxedge manifest
      validate` / `vxedge up`'s pre-provisioning check fail loudly instead
      of accepting a no-op. Zero touch to `fiveg/`, zero regression risk
      (manifests without `slice_policy` are unaffected — see
      `rejects_slice_policy_as_not_yet_implemented` and the unchanged
      `parses_and_validates_worked_example` in
      `crates/vxedge-core/src/manifest.rs`). Remove the guard when/if this
      item above is actually implemented.
- [ ] SONiC-VS twin: render slice intent to SONiC config, boot VS qcow2, verify config parity
- [ ] TrafficService: synthetic generator (pps/bandwidth/payload per profile)
- [ ] TS SDK (connect-es) generation + tonic-web enablement; minimal web dashboard repo/dir
- [ ] mTLS + OIDC auth path; viewer/operator/admin roles
- [x] Harness `impairments` rendering: tc netem per link (rate cap, latency/jitter, loss —
      100/1000BASE-T1 emulation) — **delivered 2026-08-01**. `links[].impairments` in the §6.0
      schema (`rate_mbps`/`delay_us`/`jitter_us`/`loss_pct`); the model carries loss as parts
      per million so a desired-state plan compares exactly against itself on re-apply, the same
      reason interface names are a pure function of the attachment rather than of traversal
      order.
      **The design decision**: exactly one of a link's access ports renders the netem, not
      every attachment. A two-device link's traffic already crosses two ports on this bridge —
      impairing both would double every traversal (a 250 µs wire measuring 500 µs, with nothing
      reporting it), and halving values to compensate would then be wrong for the
      single-attachment links every frozen experiment declares. The carrier is elected by
      lowest interface name (`gateway::ifnames`), so — like every other name in that module —
      it is a pure function of the attachment and cannot move by reordering endpoints.
      **The mechanism**: `tc qdisc replace ... netem` on the elected port is the
      bridge→device (egress) half. The device→bridge half cannot be shaped there — a qdisc only
      shapes egress — so it is `mirred`-redirected onto a new IFB device (`vxi-<stem>`,
      `gateway::ifnames::ingress_ifb`, derived from the access port name it serves) carrying an
      identical netem. Idempotency needed its own fix: `tc filter add` appends rather than
      converges, so a naive re-apply would stack a second redirect and silently double inbound
      impairment while `observe` kept reporting a healthy gateway — the ingress qdisc is deleted
      and rebuilt on every apply instead, which takes its filters with it and lands on exactly
      one redirect every time (a sub-millisecond unimpaired window during `up`, never during a
      measurement, since traffic starts separately via `vxedge traffic up`).
      **Refused, not silently dropped**: an impairment on a link with no provisioned access
      port (e.g. an s-nssai slice, which this reconciler doesn't provision at all — fiveg owns
      that lifecycle) is `PlanError::ImpairmentNotRenderable` rather than a manifest that
      declares a 100BASE-T1 segment and measures a clean one. The workspace validator adds two
      author-facing checks with no such runtime backstop possible: `impairments` on a link that
      binds no `slice:` (the common case — no slice means no access port, and nothing later
      would ever say why the netem never applied) and `jitter_us` without `delay_us` (nothing
      for netem to vary).
      **The declarative half landed alongside it**: `links[].harness` (`medium`, `length_m`,
      `weight_g`) — carried for a harness-reduction report, not rendered onto anything, since
      §7's own claim is that zonal E/E is justified by *less* wire and the model needs to be
      able to say how much. Independent of `impairments` and of slice-binding: a direct
      point-to-point segment has a length whether or not it crosses vxgateway.
      **Verified**: an address pinned on an endpoint (the object form exp-002 uses) and an
      impairment on the same link compose with no interaction, confirmed by a dedicated test —
      the two are orthogonal by construction (impairments are L2, addressing is L3). 133 tests
      total (was 123), clippy `-D warnings` and fmt clean. The pre-existing netns integration
      test gained a second, impairment-specific case (both directions installed, redirect count
      pinned across a re-apply) but — like the cycle test it sits beside — has not yet run
      against a real kernel; this is the same standing item the board already tracks for #3/#5/#21.
- [ ] **The seam is in** (2026-07-31): `DataPlane` — `apply_links`/`apply_slices`/
      `apply_endpoints`/`teardown`/`observe`, with `KernelBridge` as the first implementation
      and `data_plane()` the single selection point. The three apply stages are separate trait
      methods because the daemon reports progress between them, and a seam a caller must reach
      under to keep a feature is not a seam. Known limit, stated in the trait doc: the 5G
      attachment helpers are still bridge-specific free functions and join the trait when a
      second implementation needs them.
- [x] OVS data-plane variant: same GatewayPlan rendered via ovs-vsctl/OVSDB (kernel bridge
      remains the default backend) — **delivered 2026-08-01**. `network.data_plane: ovs`
      (absent means `kernel-bridge`, so every experiment that predates the field keeps
      planning and provisioning exactly as before); a manifest field, matching how
      `network.slices[].mode` already picks a mechanism, not a daemon flag. `GatewayPlan`
      carries the selection (`data_plane: DataPlaneKind`) so it's part of the provisioning
      intent rather than threaded separately through every call site.
      **What's shared and what isn't**: the HTB queue tree (`ensure_htb_root_and_default`,
      extracted from `apply_slices`) and per-slice class/filter
      (`apply_slice_htb_class`) don't know which switching fabric the uplink belongs to —
      a `tc` qdisc is the same kernel object either way — so both backends call the exact
      same functions. Only bridge creation and VLAN membership differ: the kernel bridge
      uses `ip link add ... type bridge` + `bridge vlan add`; OVS uses
      `ovs-vsctl add-br`/`add-port` + `set port ... trunks=[...]`/`tag=...`. Impairments
      (netem/IFB) needed zero changes — same reasoning, one level down.
      **A real operational difference surfaced building this, not assumed**: OVS state is
      not netns-scoped, unlike everything else `gateway_netns.rs` tests — a Linux bridge
      inside a disposable `ip netns add` vanishes with the namespace, but an OVS bridge
      lives in the system-wide `ovsdb-server` regardless of which netns created it. Verified
      empirically before writing the parsing code (a throwaway, fully-cleaned-up bridge/port
      exercising add/get/set/del) rather than guessing at `ovs-vsctl`'s JSON output shapes
      against a real system service. `gateway_ovs.rs`'s own test therefore builds a
      `GatewayPlan` directly with names that can't collide with a real gateway
      (`vxovstest`/`vxovsup`, never `vxbr0`/`vxup0`) and cleans up explicitly. **Live-verified
      against a real `ovsdb-server`**: apply, re-apply (idempotent), trunk VLAN confirmed in
      OVSDB itself (not just the plan), teardown, second teardown — all clean, zero leftover
      interfaces confirmed by hand afterward. 138 tests total (was 134), clippy `-D warnings`
      and fmt clean.
- [ ] Release binaries for vxedge/vxedged via cargo-dist (digest-recorded, §13)

### Device/node provisioning layer (pulled forward from phase 3, `docs/TASKS.md` 2026-08-01)

The API and backend surface for containers, VMs, microVMs, virtual ECUs, virtual sensors and
virtual actuators. `ComputeService` and the container backend's OCI integration were already
scoped here (§3); sensor/actuator/virtual-ECU backends move up from phase 3 by owner decision
— not gated on hardware the way the GPU-dependent phase-3 items are, only on ordering.

- [x] `ComputeService.Up/Down/List` — **delivered 2026-08-01**, see §3 for the full account
- [ ] container backend: OCI runtime integration (payload currently exec'd by the runner) — §3
- [ ] Manifest structs: `io:`, `sensors:`, `actuators:` node blocks (§6.0 — corrected from the
      stale §6.1 reference this item carried before §6.1 was deleted 2026-07-31). The domain
      model already has `DeviceRole::Sensor`/`Actuator` and per-device `SignalBinding`
      (vss/dbc); what's missing is the authored-schema surface and a provisioning backend
- [ ] Sensor/actuator virtualization: restbus simulation (DBC-driven `sensors:`/`actuators:` +
      Kuksa CAN provider), `vxplant` plant-model agent (`vxagents`)
- [ ] QEMU `io.devices` attach (virtio-gpio/i2c/spi/can) in the libvirt backend
- [ ] `backend: renode` — Renode zone backend for MCU-class virtual ECUs (register-level I/O)

## 5. Phase 3

- [ ] VehicleService: ordered composition (gateway→zones→compute→signal plane→services)
- [ ] Qt/QML operator HMI (Qt GRPC ≥6.8): live slice metrics view
- [ ] vxedge-tray: Rust SNI (StatusNotifierItem) system tray — thin vxAPI client showing
      gateway/experiment state with quick up/down; no orchestration logic (hard rule)
- [ ] vxedged systemd unit (+ socket activation where possible); packaging notes for the
      CAP_NET_ADMIN grant (AmbientCapabilities vs setcap)
- [ ] `vxedged --controller`: federation of site daemons over WireGuard+mTLS
- [ ] `traffic_profiles.source: carla` integration hook
- [ ] `type: can` links: SocketCAN/vcan provisioning, QEMU CAN controller attach, cannelloni
      CAN-over-Ethernet tunnels
- [ ] TSN gateway variant: taprio/cbs qdisc rendering + gPTP time sync (linuxptp)
- [ ] V2X groundwork: N vehicle manifests per experiment (V2V paths through vxgateway) +
      `type: rsu` node support in zone/gateway provisioning (V2I)
