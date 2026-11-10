---
name: zone-backend-contract
description: The shared contract all zone backends (container/qemu-microvm/libvirt/cloud-hypervisor/wasm-capsule/attach/qemu-mcu) and every guest image must satisfy — use when touching crates/vxedge-core/src/zones/ in vxedge, or building/publishing a zone-class guest image (Zephyr/FreeRTOS/AAOS) in vxparts. One contract, three isolation mechanisms — that's the H3 variable itself.
---

# Zone backend contract

Reference: `ARCHITECTURE.md` §9 (virtualization & zone provisioning). Code:
`crates/vxedge-core/src/zones/{backend,plan}.rs` (vxedge); guest recipes + publish
scripts live in `vxparts` (`parts/zone/<subtype>/...`, `scripts/publish/
publish-vm-bundle.sh`/`publish-vm-disk.sh`). This contract is shared across both repos — a
new guest image in `vxparts` and a new backend feature in `vxedge` must agree on it
independently, since neither repo reads the other's source.

## The backends attach to the gateway identically — only isolation differs

`Backend::{Container, QemuMicrovm, Libvirt, CloudHypervisor, WasmCapsule, Attach, QemuMcu}` (`model/mod.rs`) all
attach to the same
gateway port; per `zones/backend.rs`'s own doc comment, "the payload is exec'd... the
isolation mechanism differs — that is exactly the H3 variable." Concretely:

- `container`: named netns, veth peer moves in, renamed `eth0`, addressed from the
  manifest. With a digest-pinned or `localhost/` part, the netns belongs to a container group's
  infra container and the image runs as a member.
- `qemu-microvm`: raw QEMU `microvm` machine, direct kernel boot from the bundle
  (`vmlinuz`+`initrd.img`), virtio-net on a tap. An aarch64 bundle (`meta.json` `"arch": "aarch64"`)
  boots `qemu-system-aarch64 -M virt` instead: virtio-mmio too, console `ttyAMA0`. KVM only on a
  host of the same instruction set, else TCG; `NodeStatus.acceleration` says which.
- `libvirt`: full QEMU machine via `virsh` (`qemu:///system`), same bundle/cmdline
  contract, precreated tap (`managed='no'`).
- `cloud-hypervisor`: the same guest bundle under Cloud Hypervisor, driven over its REST API on a
  per-node Unix socket. It adopts the gateway's tap without touching its addressing, and needs a
  stable MAC.
- `wasm-capsule`: a container node's network (named netns, gateway port as `eth0`) with a WASI 0.2
  module as the payload, run by a `vxcapsule` process inside it.
- `qemu-mcu`: microcontroller firmware on an emulated board (`zones/qemu_mcu.rs`): `qemu-system-arm
  -M mps2-an385`, the board's LAN9118 NIC on a tap with the node's MAC, always TCG. The command line
  goes through semihosting (`-semihosting-config enable=on,target=native,arg=vx.ip=…,arg=vx.node=…`),
  which the firmware reads with `SYS_GET_CMDLINE`; pidfile, status and `down` are `qemu-microvm`'s.

Adding another backend (Renode, `crosvm`) or a new guest under an existing backend means
satisfying this attachment contract exactly — don't special-case gateway wiring per
guest OS.

## MCU firmware bundle contract (`qemu-mcu`)

- `firmware.elf`, loaded with `-kernel`, and `meta.json` with `machine` (or the published
  `org.vxedge.machine`): `mps2-an385`. `arch`, if present, is `armv7m`.
- The firmware reads `vx.ip=<cidr> vx.node=<id>` from the semihosting command line, takes its MAC from
  the NIC (QEMU loads it into the LAN9118), prints to UART0 and echoes UDP on :9800.

## Guest bundle contract (what every new zone-class image in vxparts must produce)

Established by `linux-probe`
(`vxparts/parts/zone/linux/stack/probe-bundle/build.sh`, migrated from `vxzones`'
`scripts/build-guest-bundle.sh` — Track P, 2026-07-28), the pattern every guest bundle
must follow, not just imitate loosely:

- `vmlinuz` + `initrd.img`, boots from the kernel cmdline `vx.ip=<address>` — guest init
  reads it and payload autostarts; no interactive provisioning step.
- SHA256-digested `meta.json` alongside the bundle (§13 reproducibility: no `latest`,
  every artifact traceable to a digest). Its `arch` (or the published `org.vxedge.arch`) names the instruction set
  (`x86_64`, the default, or `aarch64`); one image ID per instruction set, never one bundle for two.
- `vmlinuz` is a kernel QEMU's `-kernel` loads directly: on aarch64 a plain `Image` (a distribution
  kernel's EFI zboot wrapper with zstd inside is refused, so the recipe unwraps it).
- `initrd.img` is always present, and may be empty when the kernel needs no initramfs (an image that
  carries its whole system). `qemu-microvm` then leaves `-initrd` off; QEMU's arm loader refuses an empty file.
- `meta.json` `virtio_mmio`: `modern` (default, version 2; Zephyr requires it) or `legacy` (version 1, for a
  driver that speaks only that). The bundle states what its driver speaks.
- `meta.json` `cpus` and `memory_mb` size the guest; `virtualization: true` (aarch64 only) boots `virt` at EL2
  with GICv3, for a guest that is itself a hypervisor.
- Document, in the recipe's own README, exactly which `backend:` values the image
  supports — a recipe for a guest that production runs under a Type-1 hypervisor must state
  the RTOS-as-guest inversion fidelity caveat (ARCHITECTURE §9) inline, not leave it implicit.
- Agents (`vxagents`) are injected per experiment, never baked into the image
  (`vxparts/AGENTS.md`) — a guest image that assumes vxprobe/vxtraffic is already
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
