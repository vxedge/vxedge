//! Zone backends (§9): compile manifest nodes into NodePlans joined with the
//! gateway's endpoints, then run each node under its packaging backend —
//! `container` (network namespace), `qemu-microvm` (raw QEMU, microvm machine
//! type), `libvirt` (full VM via virsh), `cloud-hypervisor` (microVM over a
//! REST API), `wasm-capsule` (a WebAssembly module), `attach` (a platform's own stack) or
//! `qemu-mcu` (microcontroller firmware on an emulated board). The packaging choice is the H3
//! experimental variable; the payload and network attachment stay identical.

mod attach;
pub mod backend;
mod cloud_hypervisor;
pub mod foreign;
pub mod plan;
mod qemu_mcu;
pub mod usage;
mod wasm_capsule;

pub use backend::{
    Acceleration, GuestArch, GuestPlatform, QEMU_MICROVM_BIN, guest_platform, member_down,
    member_status, member_up, node_down, node_status, node_up, node_usage_source,
};
pub use cloud_hypervisor::{socket_too_long as ch_socket_too_long, vmm_available as ch_available};
pub use foreign::foreign_objects;
pub use plan::{
    NodePlan, ZonePlanError, missing_local_bundles, missing_local_modules, plan_nodes_from_model,
    runtime_name_of,
};
pub use qemu_mcu::QEMU_MCU_BIN;
pub use wasm_capsule::runner_available as capsule_runner_available;
