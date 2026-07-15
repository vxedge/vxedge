//! Real 5G slicing (H2): Open5GS + UERANSIM driver for `mode: s-nssai` slices.
//!
//! Same shape as the gateway backend: a pure-data plan compiled from the
//! manifest ([`FiveGPlan`]), then side-effecting stages that consume it.
//! This first increment covers the plan and deterministic config rendering
//! ([`render`]); process lifecycle (spawning the core + gNB/UEs, health,
//! teardown) lands next and reuses the pidfile-tracked process backend
//! `SvcService` already uses.
//!
//! Config schema targets **Open5GS 2.8.x** and **UERANSIM 3.2.x**. The
//! rendered config set has been proven **end to end** (2026-07-14) against
//! the containerized stack (`containers/fiveg/`, hybrid policy §15): the
//! full core (NRF/AUSF/UDM/UDR/PCF/BSF/NSSF/AMF + per-slice SMF/UPF) comes
//! up on rendered configs, the gNB completes **NG Setup**, and **both UEs
//! register and establish PDU sessions on their own S-NSSAI**, each getting
//! its slice's subnet (adas → 10.46.0.2 on uesimtun0, infotainment →
//! 10.47.0.2 on uesimtun1). Remaining before H2 numbers: vxedged pod
//! lifecycle (supervision/health/teardown), explicit subscriber documents
//! (dbctl's slice helper needs an SD), and uesimtun↔vxgateway bridging so
//! measured traffic rides the PDU sessions.

pub mod plan;
pub mod render;

pub use plan::{FiveGPlan, FiveGPlanError, SliceSession};
pub use render::{ConfigFile, render_all};
