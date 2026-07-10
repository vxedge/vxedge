# TASKS.md — vxedge/vxedge (core)

Ordered for the phase-1 critical path (see ../docs/TASKS.md). Each task should land with tests.

## 1. Scaffolding & contract (phase 1)

- [x] Cargo workspace: `vxedge-api` + `vxedge-core` + `vxedged` + `vxedge-cli` crates; CI
      (build/clippy/fmt/test + buf lint)
- [x] `proto/vxedge/v1/`: Gateway/Zone/Compute/Svc/Vehicle/Experiment/Traffic services +
      JobService (Get/Watch) + EventService (Subscribe) + TelemetryService (StreamMetrics,
      Collect); buf config; Rust codegen via vxedge-api build.rs
- [x] `vxedge-core`: experiment-manifest structs (§6.1 incl. slices/slice_policy/backend/
      realtime/carla/gpu) + serde_yaml parsing + semantic validation (slice refs, vxid
      uniqueness, slice-mode consistency, service refs) — vehicle manifest (§6.2) still TODO
- [ ] `vxedge-core`: vehicle-manifest structs (§6.2) + validation
- [x] `vxedged`: job engine (async tasks + watch-channel progress), event bus
      (EventService.Subscribe live), structured JSONL audit log of mutating calls; Gateway
      Apply/Up/Down submit real jobs (validate + plan; Up/Down fail honestly at the
      provisioning step until §2 lands)
- [ ] `vxedge-cli`: auto-spawn ephemeral daemon *(done: full clap tree, `--endpoint`, live
      `status`, and `up`/`down` submitting jobs with streamed progress + JSON output)*
- [ ] `vxedge manifest validate` subcommand (schema + semantic checks over vxedge-core;
      powers manifest-validation CI in vxperiments)

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

- [ ] libvirt backend: manifest → domain XML (vNIC→OVS tag, cputune from `realtime:`,
      hugepages, storage pool with SHA256-digest images); define/start/destroy/undefine
- [ ] `backend: qemu-microvm`: raw QEMU via QMP, `microvm` machine type (H3 arm)
- [ ] container backend for `kind: container` nodes/compute
- [ ] `ZoneService`/`ComputeService` full verbs incl. client-streaming `PushImage`

## 4. Phase 2

- [ ] Open5GS/UERANSIM driver: `mode: s-nssai` slice provisioning (H2)
- [ ] URSP-style `slice_policy`: per-node traffic-class → slice routing over concurrent PDU
      sessions (UE-side slicing; mirrors Snapdragon Auto URSP behavior)
- [ ] SONiC-VS twin: render slice intent to SONiC config, boot VS qcow2, verify config parity
- [ ] TrafficService: synthetic generator (pps/bandwidth/payload per profile)
- [ ] TS SDK (connect-es) generation + tonic-web enablement; minimal web dashboard repo/dir
- [ ] mTLS + OIDC auth path; viewer/operator/admin roles
- [ ] Harness `impairments` rendering: tc netem per link (rate cap, latency/jitter, loss —
      100/1000BASE-T1 emulation); manifest struct extension
- [ ] Release binaries for vxedge/vxedged via cargo-dist (digest-recorded, §13)

## 5. Phase 3

- [ ] VehicleService: ordered composition (gateway→zones→compute→signal plane→services)
- [ ] Qt/QML operator HMI (Qt GRPC ≥6.8): live slice metrics view
- [ ] `vxedged --controller`: federation of site daemons over WireGuard+mTLS
- [ ] `traffic_profiles.source: carla` integration hook
- [ ] `type: can` links: SocketCAN/vcan provisioning, QEMU CAN controller attach, cannelloni
      CAN-over-Ethernet tunnels
- [ ] TSN gateway variant: taprio/cbs qdisc rendering + gPTP time sync (linuxptp)
- [ ] V2X groundwork: N vehicle manifests per experiment (V2V paths through vxgateway) +
      `type: rsu` node support in zone/gateway provisioning (V2I)
- [ ] Manifest structs: `io:`, `sensors:`, `actuators:` node blocks (§6.1)
- [ ] QEMU `io.devices` attach (virtio-gpio/i2c/spi/can) in the libvirt backend
- [ ] `backend: renode` — Renode zone backend for MCU-class virtual ECUs (register-level I/O)
