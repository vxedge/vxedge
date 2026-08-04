//! vxgateway backend: compiles `network.slices` + zone endpoints into a
//! provisioning plan (pure data), then renders it onto the Linux networking
//! stack — bridge + VLAN filtering, veth endpoints, tc HTB queue tree.
//!
//! H1 minimum (TASKS §2): bridge/VLAN/HTB. WireGuard/VXLAN/FRR/nftables trail
//! within phase 1. Default backend is the kernel bridge — every frozen H1/H3/H4 number was
//! measured on it; the OVS variant (`network.data_plane: ovs`) is a second implementation
//! behind the same [`plan::GatewayPlan`], not a replacement — see AGENTS.md.

pub mod ifnames;
pub mod netlink;
pub mod netops;
pub mod plan;
pub mod provision;

pub use netops::{NetOpsError, Shell};
pub use plan::{GatewayPlan, PlanError};
pub use provision::{
    DataPlane, GatewayObservation, KernelBridge, Ovs, SliceObservation, apply, data_plane, observe,
    teardown,
};
