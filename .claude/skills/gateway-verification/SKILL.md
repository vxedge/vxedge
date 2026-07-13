---
name: gateway-verification
description: How to verify vxgateway/zone provisioning changes in vxedge — netns integration tests, UDS smoke test, doctor preflight, and idempotency rules. Use when touching gateway/zone backends, reconciler, or CLI up/down/status.
---

# Gateway & zone verification (vxedge)

Reference: ARCHITECTURE.md §7 (vxgateway), §9 (virtualization backends), §5 (reconciler).

## Verification loop (bind to these, don't reinvent)

1. **Unprivileged first**: `cargo test --workspace` must pass with zero root — privileged
   gateway/zone tests are `#[ignore]`d for exactly this reason (rust-patterns ch. 14).
2. **Netns integration test**: `sudo -E env "PATH=$PATH" cargo test -p vxedge-core --test
   gateway_netns -- --ignored --nocapture` (`crates/vxedge-core/tests/gateway_netns.rs`) —
   the CI job for any bridge/VLAN/tc change. Run it locally before pushing if you touched
   `gateway/plan.rs` or the reconciler's observe/verify path.
3. **UDS smoke test**: `vxedged --listen unix:///tmp/vxedge.sock` then `vxedge --endpoint
   unix:///tmp/vxedge.sock status` — catches wiring bugs unit tests miss (the zombie-process
   bug in `SvcService` was found this way, not by unit tests).
4. **Host preflight**: `vxedge doctor --tier N --json` (`crates/vxedge-cli/src/doctor.rs`)
   before any live run — validates the tier-N host requirements from
   `docs/TESTBED-SETUP.md` with actionable fix hints.
5. **Manifest changes**: `vxedge manifest validate --json <manifest>` — same read-only check
   `vxedge up` runs before provisioning, wraps `ExperimentManifest::from_yaml` directly.

## Idempotency rule (non-negotiable)

`up|down` compare desired (manifest) vs observed (libvirt/OVS/netns state) and converge —
running `up` twice must be a no-op, and a job that can't finish reports FAILED honestly,
never a hollow SUCCEEDED. Any backend change needs a re-`up` regression check, not just a
first-run check (`manifest.rs` `Backend` enum: `libvirt` default for vm/rtimage,
`qemu-microvm` the escape hatch — both converge through the same `GatewayPlan`/reconciler).

## Fidelity caveats to preserve, not "fix"

The platform is production-*representative*, not safety-certified (§14). Don't phrase test
coverage or backend choices as ASIL/safety-certification claims — state what's measured
(freedom-from-interference via the hypervisor fidelity ladder, exp-004) and what's a stated
gap (shared-host gateway, non-PREEMPT_RT kernel, QNX-as-guest inversion).
