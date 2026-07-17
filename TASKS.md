# TASKS.md — vxedge/vxedge (core)

Ordered for the phase-1 critical path (see ../docs/TASKS.md). Each task should land with tests.

## 1. Scaffolding & contract (phase 1)

- [x] Cargo workspace: `vxedge-api` + `vxedge-core` + `vxedged` + `vxedge-cli` crates; CI
      (build/clippy/fmt/test + buf lint)
- [x] `proto/vxedge/v1/`: Gateway/Zone/Compute/Svc/Vehicle/Experiment/Traffic services +
      JobService (Get/Watch) + EventService (Subscribe) + TelemetryService (StreamMetrics,
      Collect); buf config; Rust codegen via vxedge-api build.rs
- [x] `vxedge-core`: experiment-manifest structs (§6.1 incl. slices/slice_policy/backend/
      realtime/carla/gpu/`telemetry.prometheus.optional_scrape_targets`) + serde_yaml
      parsing + semantic validation (slice refs, vxid uniqueness, slice-mode consistency,
      service refs) — vehicle manifest (§6.2) still TODO
- [ ] `vxedge-core`: vehicle-manifest structs (§6.2) + validation
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
- [ ] `ComputeService` verbs; client-streaming `PushImage` (image store)

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
- [ ] `telemetry` service type in the generic process backend (today: CLI-driven only)

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
- [ ] **Image-digest recording into results metadata.** Sized 2026-07-17,
      not started: not a small add — there is no results-metadata-writing
      code in `vxedge-core` yet to hook a `podman inspect --format
      '{{.RepoDigests}}'`-style lookup into. `vxedge telemetry collect`
      (ARCHITECTURE.md §16, the natural home for this) isn't implemented
      here; the actual results-artifact pipeline may live in `vxtelemetry`
      or `vxperiments` instead. First task before writing any digest-lookup
      code is finding/reading whichever of those repos actually owns
      results-archive writing today. Not H2-blocking.
- [ ] **URSP-style `slice_policy`.** Sized 2026-07-17, not started: not a
      small add — the manifest schema (ARCHITECTURE.md §6.1) is `match:
      <traffic class|dscp|app id> → slice: <slice-id>` **per zone node**,
      meaning a single real compute node needs *multiple concurrent PDU
      sessions* (one per slice) with traffic-class-based policy routing
      selecting between them. But `FiveGPlan`'s current model is 1 UE = 1
      `SliceSession` = 1 slice — no UE has more than one PDU session.
      Supporting URSP means: (1) reworking `FiveGPlan`/`SliceSession` to let
      one UE carry several concurrent sessions across different S-NSSAIs,
      (2) rendering multiple `sessions:` entries into a single `ue-*.yaml`
      (UERANSIM supports this; not yet exercised here), (3) real DSCP/`tc`
      policy routing on the node side selecting which `uesimtun<i>` a given
      traffic class rides. This is a real feature needing its own design
      pass, not a follow-up patch — per-node traffic-class → slice routing
      over concurrent PDU sessions (UE-side slicing; mirrors Snapdragon Auto
      URSP behavior). Not H2-blocking.
- [ ] SONiC-VS twin: render slice intent to SONiC config, boot VS qcow2, verify config parity
- [ ] TrafficService: synthetic generator (pps/bandwidth/payload per profile)
- [ ] TS SDK (connect-es) generation + tonic-web enablement; minimal web dashboard repo/dir
- [ ] mTLS + OIDC auth path; viewer/operator/admin roles
- [ ] Harness `impairments` rendering: tc netem per link (rate cap, latency/jitter, loss —
      100/1000BASE-T1 emulation); manifest struct extension
- [ ] OVS data-plane variant: same GatewayPlan rendered via ovs-vsctl/OVSDB (kernel bridge
      remains the default backend)
- [ ] Release binaries for vxedge/vxedged via cargo-dist (digest-recorded, §13)

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
- [ ] Manifest structs: `io:`, `sensors:`, `actuators:` node blocks (§6.1)
- [ ] QEMU `io.devices` attach (virtio-gpio/i2c/spi/can) in the libvirt backend
- [ ] `backend: renode` — Renode zone backend for MCU-class virtual ECUs (register-level I/O)
