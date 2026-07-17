---
name: zone-backend-contract
description: The shared contract all three zone backends (container/qemu-microvm/libvirt) and every guest image must satisfy — use when touching crates/vxedge-core/src/zones/ in vxedge, or building/publishing a guest image (Zephyr/FreeRTOS/QNX/AAOS) in vxzones. One contract, three isolation mechanisms — that's the H3 variable itself.
---

# Zone backend contract

Reference: `ARCHITECTURE.md` §9 (virtualization & zone provisioning). Code:
`crates/vxedge-core/src/zones/{backend,plan}.rs` (vxedge); guest recipes + publish
scripts live in `vxzones`. This contract is shared across both repos — a new guest image
in `vxzones` and a new backend feature in `vxedge` must agree on it independently, since
neither repo reads the other's source.

## The three backends attach to the gateway identically — only isolation differs

`Backend::{Libvirt, QemuMicrovm, Container}` (`manifest.rs`) all attach to the same
gateway port; per `zones/backend.rs`'s own doc comment, "the payload is exec'd... the
isolation mechanism differs — that is exactly the H3 variable." Concretely:

- `container`: named netns, veth peer moves in, renamed `eth0`, addressed from the
  manifest.
- `qemu-microvm`: raw QEMU `microvm` machine, direct kernel boot from the bundle
  (`vmlinuz`+`initrd.img`), virtio-net on a tap.
- `libvirt`: full QEMU machine via `virsh` (`qemu:///system`), same bundle/cmdline
  contract, precreated tap (`managed='no'`).

Adding a fourth backend (Renode, phase 3) or a new guest under an existing backend means
satisfying this attachment contract exactly — don't special-case gateway wiring per
guest OS.

## Guest bundle contract (what every new image in vxzones must produce)

Established by `linux-probe` (`vxzones/scripts/build-guest-bundle.sh`), the pattern every
future guest (QNX SDP 8, AAOS, real Zephyr/FreeRTOS) must follow, not just imitate
loosely:

- `vmlinuz` + `initrd.img`, boots from the kernel cmdline `vx.ip=<address>` — guest init
  reads it and payload autostarts; no interactive provisioning step.
- SHA256-digested `meta.json` alongside the bundle (§13 reproducibility: no `latest`,
  every artifact traceable to a digest).
- Document, in the recipe's own README, exactly which `backend:` values the image
  supports — QNX's recipe in particular must state the QNX-Hypervisor-inversion fidelity
  caveat (`vxzones/AGENTS.md`) inline, not leave it implicit.
- Agents (`vxagents`) are injected per experiment, never baked into the image
  (`vxzones/AGENTS.md`) — a guest image that assumes vxprobe/vxtraffic is already
  present breaks the inject-per-experiment model.

## Ordering contract (both repos must respect it, neither enforces it for you)

`gateway up` before `zone up`; `zone down` before `gateway down`. Deleting a container
netns destroys its veth pair, so re-running `vxedge up` (idempotent) must restore
endpoints after a `zone down` — a new backend or guest that breaks this ordering breaks
every existing experiment runner script, not just its own test.

## Plan-join invariant

`NodePlan` (zones/plan.rs) is joined 1:1 with `GatewayPlan`'s endpoints using the *same
deterministic traversal order* — asserted, not assumed (`EndpointMismatch` is a hard
plan-construction error). If you change iteration order anywhere in either plan builder,
you break this join silently unless the assertion catches it — don't reorder manifest
traversal without checking both plan builders together.
