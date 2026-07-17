---
name: open5gs-ueransim-patterns
description: Correctness patterns for the containerized 5G core and RAN (Open5GS/UERANSIM) — config rendering (vxedge-core::fiveg) AND UE-netns networking/deployment gotchas (policy routing, socket binding, per-UE namespacing). Use when touching fiveg/, anything that reads/writes S-NSSAI slice config, or any process (vxprobe or otherwise) that runs inside a UE's pod/netns.
---

# 5G core & RAN patterns (Open5GS / UERANSIM)

Reference: `ARCHITECTURE.md` §7 (H2 real slicing), §15 phase 2; `docs/STATUS.md`'s dated
H2 entries are the primary source for the config-rendering lessons below — read them
before extending `crates/vxedge-core/src/fiveg/` (plan.rs → render.rs → lifecycle.rs →
subscriber.rs). The UE-netns networking lessons below are equally load-bearing for
anything that runs *inside* a UE pod, not just the fiveg module itself.

## The plan is the only source of NF config (never re-derive)

- `FiveGPlan` (plan.rs) is built once from `network.slices` (`mode: s-nssai`); every NF
  config file and every subscriber document is *rendered from the plan struct*, never
  re-parsed from the manifest YAML or re-typed by hand. `subscriber.rs` writes MongoDB
  documents straight from `FiveGPlan` fields for exactly this reason — a second parse
  path is a second place for the s_nssai/dnn/tun_dev triad to drift apart.
- S-NSSAI `sd` is normalized to lowercase, exactly 6 hex digits, at plan-build time
  (`normalize_sd`) — validate here, not downstream. `sst`/`sd` uniqueness across slices
  is a plan-construction error (`FiveGPlanError`), not a runtime surprise.
- Per-slice UPF tun devices (`vxtun<i>`) are mandatory, not optional: Open5GS's default
  `ogstun` is single-instance, so two slices sharing it silently collapses S-NSSAI
  differentiation. Same pattern on the RAN side: `uesimtun<i>` on the UE, `vxtun<i>` on
  the UPF — the plan asserts they're joined 1:1, don't hand-roll the index.

## Schema landmines already paid for (don't rediscover)

Found against live Open5GS 2.8.0, not from docs — the schema is stricter or differently
shaped than upstream examples suggest in these specific ways:

- AMF config needs `t3512` even though it reads like an optional timer field.
- NSSF's `s_nssai` mapping is a single object, not a list — copying the multi-entry
  shape from other NF configs breaks NSSF specifically.
- SMF's `dnn` block nests *inside* `s_nssai`, not as a sibling key.
- `open5gs-dbctl`'s SD-less subscriber helper (`add_ue_with_apn`) hardcodes
  `sst: NumberInt(1)` and drops `sd` regardless of the slice's real SST — this is why
  `subscriber.rs` writes MongoDB documents directly instead of shelling out to `dbctl`.
  If you're ever tempted to reach for `dbctl` again for anything beyond schema
  reference, don't.
- Mongo's `session` field on a subscriber document is an **array**, even for a single
  PDU session config. A malformed bare-object write is accepted silently on insert and
  only fails later, at query time, with an opaque
  `OpenAPI_snssai_info_convertToJSON() failed [dnn_infos]` — and it breaks *every*
  subscriber's PDU sessions, not just the one you're testing. If NG Setup succeeds but
  PDU Session Establishment fails for all UEs, check the document shape first.

## UE-netns networking: never bind a reflector/service to `0.0.0.0`

Found 2026-07-17 during H2 probe-path live verification (`fiveg::lifecycle::ue_probe_up`).
**This is a network-provisioning pattern, not a language bug** — it reproduces
identically in Rust, C, Go, or Python, for any process binding `INADDR_ANY` inside a
UE's pod/netns. Full write-up with the diagnosis trail: ask for
`bug-udp-reply-routing-ueransim.md` if you need the raw evidence (interface counters,
`ip rule`/`ip route` dumps) rather than just the pattern below.

- **Symptom:** a UDP reflector bound to `0.0.0.0:<port>` inside a UE's netns received
  every probe (confirmed via `uesimtun0`'s own RX counter — moved exactly in step with
  packets sent) but almost no replies got back to the sender. Looked like packet loss;
  wasn't. Raw ICMP `ping` to the same UE IP over the same path succeeded at 0% loss,
  which is what points at the *reply* path specifically, not the network path.
- **Root cause:** `nr-ue` installs a per-UE source-based policy route on registration —
  `ip rule: from <UE-IP> lookup rt_uesimtun0`. That rule only fires for traffic that
  *already has* a fixed source address matching `<UE-IP>`. A socket bound to
  `INADDR_ANY` has no fixed source at `sendto()` time, so the kernel resolves the reply's
  egress via the **main table's default route** first — in this environment, UERANSIM's
  own unrelated `tap0` management interface, not `uesimtun0`. Replies silently exit the
  wrong interface and never traverse the PDU session tunnel back to the sender.
- **Fix:** never bind `0.0.0.0` inside a UE netns. Discover the UE's actual PDU-session
  address (`uesimtun0`'s assigned IP, e.g. via `ip -4 -o addr show uesimtun0` inside the
  netns) and bind explicitly to it. Binding to a specific local address forces the
  kernel to use it as the fixed source for every packet the socket sends, which makes
  the `from <UE-IP> lookup rt_uesimtun0` rule apply unconditionally.
- **The discovery step must be a bounded poll, not a fixed sleep** — binding before the
  address exists fails outright (`EADDRNOTAVAIL`), unlike some other fixed-delay phase
  settling elsewhere in this codebase that degrades gracefully when "not quite long
  enough." UE registration + PDU Session Establishment is an asynchronous live handshake
  with no fixed cost; poll `uesimtun0` up to a bounded timeout (~15s) instead of sleeping
  a guessed duration.
- **Generalizes beyond vxprobe:** any future process deployed inside a UE pod — a
  different probe, an agent, a service — hits this identically if it binds `0.0.0.0` for
  "convenience." Bind to the discovered tunnel address, always.

## UE-netns networking: one UE per netns, not shared

UERANSIM hardcodes routing table ID `1000`/name `rt_uesimtun0` for a UE's per-session
default route, independent of interface index. Two UEs sharing one network namespace
collide the moment the second one registers (`ip route add default ... table
rt_uesimtun0` → `RTNETLINK answers: File exists`). This is UERANSIM's own intended
deployment model, not a workaround: **one UE per pod/netns, always** — `vxedge` gives
every UE its own pod for exactly this reason. Don't try to co-locate multiple UEs in one
netns to save pod overhead; it breaks at the second UE's registration, not at setup time.

## Verification discipline

- Containerized only (`vxedge/containers/fiveg/`, vx5g-core/vx5g-ran images) — no host
  Open5GS/UERANSIM install, ever (Track C policy, `ARCHITECTURE.md` §15). If you find
  yourself apt-installing Open5GS to debug, stop and reproduce inside the pod instead.
- "Daemons start and stay running" is not proof of anything — the actual gate is NG
  Setup + PDU Session Establishment landing each UE on *its own slice's subnet*
  (`10.4<6+i>.0.2` pattern), **and** real `vxprobe send`/`reflect` round-trips through
  the actual bridge + PDU-session path (not `ping`) showing real RTT numbers — `ping`
  alone can pass while the actual application-level reply path is silently broken (see
  the `0.0.0.0` binding bug above; ICMP wasn't affected because kernel-generated ICMP
  replies use different source-selection than a userspace `sendto()` on an unbound
  socket).
- Re-verify live after any subscriber/session-document or UE-networking change:
  automated integration test first (`fiveg_podman.rs`, `#[ignore]`d), then a manual gNB +
  both-UE spot-check — this exact two-step caught the `session`-array bug the unit tests
  alone didn't, and is the same discipline that caught the `0.0.0.0` binding bug (via
  interface packet counters, not just aggregate loss percentage).
