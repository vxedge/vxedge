//! Zone backends (§9): compile manifest nodes into NodePlans joined with the
//! gateway's endpoints, then run each node under its packaging backend —
//! `container` (network namespace), `qemu-microvm` (raw QEMU, microvm machine
//! type), `libvirt` (full VM via virsh), `cloud-hypervisor` (microVM over a
//! REST API), or `wasm-capsule` (a WebAssembly module). The packaging choice is the H3
//! experimental variable; the payload and network attachment stay identical.

mod attach;
pub mod backend;
mod cloud_hypervisor;
pub mod foreign;
pub mod plan;
mod wasm_capsule;

pub use backend::{
    QEMU_MICROVM_BIN, member_down, member_status, member_up, node_down, node_status, node_up,
};
pub use cloud_hypervisor::{socket_too_long as ch_socket_too_long, vmm_available as ch_available};
pub use foreign::foreign_objects;
pub use plan::{
    NodePlan, ZonePlanError, missing_local_bundles, missing_local_modules, plan_nodes_from_model,
};
pub use wasm_capsule::runner_available as capsule_runner_available;
