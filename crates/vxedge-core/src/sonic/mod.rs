//! SONiC-VS production-NOS twin (`docs/TASKS.md` #16): renders vxgateway's own slice/VLAN
//! intent onto SONiC's config schema, so the manifest that declares a slice topology can prove
//! the same intent expresses identically on a real production NOS — `ARCHITECTURE.md` §7's own
//! claim. **Not** a second data plane vxedge measures traffic through: the SONiC-VS data plane
//! is explicitly "functional but not timing-representative" (§7), so every latency/jitter number
//! this project has published still runs against the Linux-native path
//! (`network.data_plane: kernel-bridge|ovs`). This twin exists to prove config portability, not
//! to carry traffic, and is not part of `experiment up`'s provisioning sequence.
//!
//! Two stages, the same shape `gateway`/`fiveg` already use: a pure-data plan compiled from
//! [`crate::gateway::GatewayPlan`] ([`plan::SonicPlan`]), and deterministic rendering onto
//! SONiC's config format ([`render::render`]) — `config_db.json`, the format a SONiC (or
//! SONiC-VS) instance loads at boot (`config load`, or as the initial
//! `/etc/sonic/config_db.json`), not `minigraph.xml` (built for real fabric deployment, far
//! more than a slice/VLAN twin needs to claim).
//!
//! **Delivered here**: the renderer, pure data, unit-tested, no VM required.
//! **Not yet delivered**: booting the actual community qcow2 and verifying live parity against
//! it — the other half of `docs/TASKS.md` #16's SONiC-VS item. That needs the real image (not
//! vendored in this repo) and a multi-minute VM boot, so it is tracked as its own follow-on step
//! rather than assumed proven because the renderer exists and its output looks right.

pub mod plan;
pub mod render;
