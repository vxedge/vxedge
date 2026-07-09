# TASKS.md — vxedge/vxedge (core)

Ordered for the phase-1 critical path (see ../docs/TASKS.md). Each task should land with tests.

## 1. Scaffolding & contract (phase 1)

- [ ] Cargo workspace: `vxedge-core`, `vxedged`, `vxedge-cli` crates; CI (build/clippy/fmt/test)
- [ ] `proto/vxedge/v1/`: Gateway/Zone/Compute/Svc/Vehicle/Experiment/Traffic services +
      JobService (Get/Watch) + EventService (Subscribe) + TelemetryService (StreamMetrics,
      Collect); buf config + codegen in CI
- [ ] `vxedge-core`: manifest structs (experiment §6.1, vehicle §6.2) + serde_yaml parsing +
      semantic validation (slice refs, vxid uniqueness, role enum, backend enum)
- [ ] `vxedged` skeleton: tonic server on UDS, job engine (async tasks + progress channel),
      event bus, structured audit log of mutating calls
- [ ] `vxedge-cli`: clap tree matching the canonical commands; `--endpoint`; auto-spawn
      ephemeral daemon; human + `--json` output

## 2. vxgateway backend (phase 1 — the research-critical piece)

- [ ] netns/bridge-OVS provisioning via rtnetlink/OVSDB: trunk + per-slice VLANs from
      `network.slices`
- [ ] tc HTB queue trees per slice (min_rate/max_rate/priority) — H1 mechanism
- [ ] nftables per-slice firewall; WireGuard link setup; VXLAN inter-host links
- [ ] FRR config rendering (BGP optional path)
- [ ] `GatewayService.Apply/Up/Down/Status` wired to reconciler; idempotency test: double-up,
      double-down, up-after-partial-failure
- [ ] netns-based integration test suite (CI-runnable)

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
