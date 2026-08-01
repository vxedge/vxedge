---
name: network-provisioning-patterns
description: How to write (not just verify) vxgateway's Linux SDN provisioning code — use when touching crates/vxedge-core/src/gateway/ (netops, plan, provision) or extending it toward OVS/nftables/WireGuard/VXLAN/FRR/TSN. Complements gateway-verification, which covers testing the result.
---

# Network provisioning patterns (vxgateway)

Reference: `ARCHITECTURE.md` §7 (vxgateway SDN model). See also `gateway-verification`
for the test loop this code must pass — this skill is about writing it correctly in the
first place. Code: `crates/vxedge-core/src/gateway/{netops,plan,provision}.rs`.

## Current backend is a deliberate deviation from the architecture doc — know which one you're in

`ARCHITECTURE.md` describes the target as Rust `rtnetlink`/OVSDB, "no shell templating."
The current implementation (`netops.rs`, doc comment: "deliberate engineering call,
recorded in TASKS") shells out to iproute2 (`ip`/`bridge`/`tc`) instead, with a
pure-rtnetlink port as an open, non-blocking phase-1 trailing item
(`vxedge/TASKS.md` §2). Both are correct to build against **for now**:

- Never construct a shell command by string interpolation. Every `Shell` invocation is
  an argument vector of already-validated, generated tokens — that's what makes shelling
  out safe here instead of a command-injection surface. If you're building a command
  string with `format!`, stop.
- Reads use `-json` (`ip -json ...`) and get parsed, not scraped from human-readable
  output — `tc`/iproute2 version drift across hosts is a known risk (`STATUS.md` risk
  list), and JSON output is the mitigation. Any new observation path follows the same
  rule.
- The `Shell` struct's `netns` field and `sudo` prefix flag are how privilege and
  isolation get threaded through — for CI (netns, unprivileged where possible) and for
  `vxedged`'s real deployment (CAP_NET_ADMIN via cached/NOPASSWD sudo, not `setcap` —
  `setcap` doesn't propagate to the child processes this shells out to, found live
  2026-07-17). Reuse it; don't spawn `Command` directly elsewhere in gateway code.
- When the rtnetlink port happens, the invariant that survives is the plan/reconcile
  layer above `netops.rs` — don't let backend-specific detail leak upward into
  `GatewayPlan`/`provision.rs` now, or the port becomes a rewrite instead of a swap.

## Two DataPlane implementations now — kernel bridge is still the default

`gateway/mod.rs`: kernel bridge is the default, OVS (`Ovs`, `network.data_plane: ovs`)
is the second implementation behind the same `GatewayPlan`, delivered 2026-08-01. H1's
frozen numbers were measured against the kernel-bridge path and must stay reproducible
against it — the OVS variant is additive, selected by a manifest field that defaults to
`kernel-bridge` when absent, so every experiment that predates the field keeps planning
and provisioning byte-for-byte as before.

What the two backends actually share, and why it's safe to share it: the HTB queue tree
(`ensure_htb_root_and_default`/`apply_slice_htb_class` in `provision.rs`) and the
impairment/netem code act on the uplink's or access port's own kernel qdisc, which exists
identically regardless of which switching fabric the interface is enslaved to — a `tc`
qdisc doesn't know or care whether its device is a Linux bridge port or an OVS port. Only
bridge creation and VLAN membership are backend-specific: the kernel bridge uses `ip link
add ... type bridge` + `bridge vlan add`; OVS uses `ovs-vsctl add-br`/`add-port` +
`set port ... trunks=[...]`/`tag=...`. If you add a third backend, extract further only
when it *also* needs to share the tc-based code — don't generalize preemptively.

**OVS state is not namespace-isolated, unlike everything else this skill covers.** A
Linux bridge created inside a disposable `ip netns add` vanishes with the namespace,
because kernel bridges are netns-scoped. OVS bridges are not: they live in the
system-wide `ovsdb-server` regardless of which netns the `ovs-vsctl` command that
created them ran from. `gateway_ovs.rs`'s own test therefore builds a `GatewayPlan`
directly with names that can't collide with a real gateway (never `vxbr0`/`vxup0`) and
cleans up explicitly, rather than relying on namespace teardown — read that test's module
doc before writing another OVS-touching test, since reusing the netns pattern here would
silently mutate the real host's OVS config instead of a disposable copy of it.

## Idempotency is the actual deliverable, not a nice-to-have

- `up|down` must be a no-op on double-apply — this is what makes the reconciler's
  observe-vs-desired model work at all, and it's directly load-bearing for the OEM-trust
  bar (`testbed-quality`: "up|down idempotent — double-apply is a no-op, verified by
  tests"). Three idempotency bugs were already found and fixed by the netns CI suite
  before H1 — treat a passing `cargo test` without the `--ignored` netns suite as
  incomplete, not green.
- Any new provisioning primitive (nftables rule, WireGuard peer, VXLAN tunnel, FRR
  session, taprio/gPTP config) needs an `observe` path before it needs an `apply` path —
  if you can't observe current state, you can't reconcile against it, and "verified"
  means "verified by an integration test that applies twice," not "applied once and
  looked fine."

## Slice QoS specifics

- HTB queue-tree construction (`tc class add ... htb rate ... ceil ... prio ...`) is
  per-slice, keyed off `network.slices[].qos` — min-rate is a *guarantee*, not a
  suggestion; H1's isolation claim rests on it holding under contention, so a
  regression here is a regression in the paper's own headline number, not just a test
  failure.

## Cross-reference: UE-side networking is a different skill

Policy-routing/socket-binding gotchas *inside a UERANSIM UE's netns* (e.g. never bind a
reflector to `0.0.0.0` there) are covered in `open5gs-ueransim-patterns`, not here — this
skill is the gateway/host side of the network stack (bridge, VLAN, tc, and their
phase-2 successors); that one is the RAN/UE side. Both are "network provisioning," but
the failure modes and the tooling involved don't overlap.
