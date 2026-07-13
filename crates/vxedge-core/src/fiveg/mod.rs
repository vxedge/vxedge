//! Real 5G slicing (H2): Open5GS + UERANSIM driver for `mode: s-nssai` slices.
//!
//! Same shape as the gateway backend: a pure-data plan compiled from the
//! manifest ([`FiveGPlan`]), then side-effecting stages that consume it.
//! This first increment covers the plan and deterministic config rendering
//! ([`render`]); process lifecycle (spawning the core + gNB/UEs, health,
//! teardown) lands next and reuses the pidfile-tracked process backend
//! `SvcService` already uses.
//!
//! Config schema targets **Open5GS 2.7.x** and **UERANSIM 3.2.x**. Rendered
//! configs are unit-tested for structure and determinism but have not yet
//! been validated against live binaries (not installed on the dev host —
//! `vxedge doctor --tier 2` reports them); that validation is a tracked
//! TASKS.md item and gates any H2 measurement claim.

pub mod plan;
pub mod render;

pub use plan::{FiveGPlan, FiveGPlanError, SliceSession};
pub use render::{ConfigFile, render_all};
