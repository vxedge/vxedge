//! Real 5G slicing (H2): Open5GS + UERANSIM driver for `mode: s-nssai` slices.
//!
//! Same shape as the gateway backend: a pure-data plan compiled from the
//! manifest ([`FiveGPlan`]), then side-effecting stages that consume it.
//! This first increment covers the plan and deterministic config rendering
//! ([`render`]); process lifecycle (spawning the core + gNB/UEs, health,
//! teardown) lands next and reuses the pidfile-tracked process backend
//! `SvcService` already uses.
//!
//! Config schema targets **Open5GS 2.8.x** and **UERANSIM 3.2.x** — every
//! rendered config is validated (2026-07-14) against the containerized stack
//! (`containers/fiveg/`: vx5g-core / vx5g-ran images, hybrid policy in
//! ARCHITECTURE §15): all Open5GS NFs reach `initialize...done` and stay
//! running (UPF creates its per-slice vxtun device under NET_ADMIN +
//! /dev/net/tun), nr-gnb parses and reaches the NGAP SCTP connect, nr-ue
//! parses and runs its NAS state machine. Remaining before H2 numbers:
//! process lifecycle (pod supervision), AUSF/UDM/UDR + subscriber
//! provisioning (open5gs-dbctl), uesimtun↔gateway bridging.

pub mod plan;
pub mod render;

pub use plan::{FiveGPlan, FiveGPlanError, SliceSession};
pub use render::{ConfigFile, render_all};
