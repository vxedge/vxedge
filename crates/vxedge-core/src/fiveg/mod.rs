//! Real 5G slicing (H2): Open5GS + UERANSIM driver for `mode: s-nssai` slices.
//!
//! Same shape as the gateway backend: a pure-data plan compiled from the
//! manifest ([`FiveGPlan`]), then side-effecting stages that consume it.
//! This first increment covers the plan and deterministic config rendering
//! ([`render`]); process lifecycle (spawning the core + gNB/UEs, health,
//! teardown) lands next and reuses the pidfile-tracked process backend
//! `SvcService` already uses.
//!
//! Config schema targets **Open5GS 2.8.x** and **UERANSIM 3.2.x**.
//! Validation state (2026-07-14): every rendered Open5GS config was run
//! against live 2.8.0 daemons — AMF/NSSF parse fully (fail only on ports
//! held by concurrently running instances), SMFs parse *and start*, UPFs
//! parse and stop only at unprivileged tun creation (the lifecycle stage's
//! job). UERANSIM configs remain schema-unvalidated until nr-gnb/nr-ue are
//! built on the host (`vxedge doctor --tier 2`); that gap gates any H2
//! measurement claim.

pub mod plan;
pub mod render;

pub use plan::{FiveGPlan, FiveGPlanError, SliceSession};
pub use render::{ConfigFile, render_all};
