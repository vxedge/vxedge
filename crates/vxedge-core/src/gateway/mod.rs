//! vxgateway backend: compiles `network.slices` + zone endpoints into a
//! provisioning plan (pure data), then renders it onto the Linux networking
//! stack — bridge + VLAN filtering, veth endpoints, tc HTB queue tree.
//!
//! H1 minimum (TASKS §2): bridge/VLAN/HTB. WireGuard/VXLAN/FRR/nftables trail
//! within phase 1. Backend is the kernel bridge (not OVS) — see AGENTS.md.

pub mod netops;
pub mod plan;
pub mod provision;

pub use netops::{NetOpsError, Shell};
pub use plan::{GatewayPlan, PlanError};
pub use provision::{GatewayObservation, SliceObservation, apply, observe, teardown};
