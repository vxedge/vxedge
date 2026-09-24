//! SONiC-VS production-NOS twin (`docs/ROADMAP.md` #16): renders vxgateway's own slice/VLAN
//! intent onto SONiC's config schema, so the manifest that declares a slice topology can prove
//! the same intent expresses identically on a real production NOS — `ARCHITECTURE.md` §7's own
//! claim. **Not** a second data plane vxedge measures traffic through: the SONiC-VS data plane
//! is explicitly "functional but not timing-representative" (§7), so every latency/jitter number
//! this project has published still runs against the Linux-native path
//! (`network.data_plane: kernel-bridge|ovs`). This twin exists to prove config portability, not
//! to carry traffic, and is not part of `experiment up`'s provisioning sequence.
//!
//! Three stages, the same shape `gateway`/`fiveg` already use: a pure-data plan compiled from
//! [`crate::gateway::GatewayPlan`] ([`plan::SonicPlan`]), deterministic rendering onto SONiC's
//! config format ([`render::render`]) — `config_db.json`, the format a SONiC (or SONiC-VS)
//! instance loads at boot (`config load`, or as the initial `/etc/sonic/config_db.json`), not
//! `minigraph.xml` (built for real fabric deployment, far more than a slice/VLAN twin needs to
//! claim) — and a side-effecting [`lifecycle`] stage that boots the image and talks to it over
//! its serial console, mirroring `fiveg`'s own plan/render/lifecycle split.
//!
//! **Delivered**: the renderer (pure data, unit-tested, no VM required); the image itself
//! sourced, boot-verified, and published via `vxparts/parts/gw/nos/os/sonic-vs`; live config
//! parity manually proven end to end first (a real `config load` against a real booted
//! instance, read back exactly matching what the renderer produced — see that part's own
//! README) via a Python/`pexpect` script, then ported to a native driver
//! ([`lifecycle::Console`], process boot/teardown in [`lifecycle`]) and native business logic
//! ([`config_check::verify_config_parity`]) so the daemon needs no Python dependency at
//! runtime.
//!
//! **Not yet delivered**: hooking any of this into `GatewayService`/`experiment up` — today it
//! is a callable library, not wired into any RPC or CLI verb. A real, separately-scoped
//! follow-on — `docs/ROADMAP.md` tracks it, not assumed done because the pieces underneath exist.

pub mod config_check;
pub mod lifecycle;
pub mod plan;
pub mod render;
