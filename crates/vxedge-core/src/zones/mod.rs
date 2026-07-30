//! Zone backends (§9): compile manifest nodes into NodePlans joined with the
//! gateway's endpoints, then run each node under its packaging backend —
//! `container` (network namespace), `qemu-microvm` (raw QEMU, microvm machine
//! type), or `libvirt` (full VM via virsh). The packaging choice is the H3
//! experimental variable; the payload and network attachment stay identical.

pub mod backend;
pub mod plan;

pub use backend::{node_down, node_status, node_up};
pub use plan::{NodePlan, ZonePlanError, plan_nodes_from_model};
