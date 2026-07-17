//! Real 5G slicing (H2): Open5GS + UERANSIM driver for `mode: s-nssai` slices.
//!
//! Same shape as the gateway backend: a pure-data plan compiled from the
//! manifest ([`FiveGPlan`]), deterministic config rendering ([`render`]),
//! and a side-effecting [`lifecycle`] stage that brings the rendered configs
//! up as a running podman pod (mirrors `services.rs`'s pidfile-tracked
//! process backend, one level up: one **pod**, not one process).
//!
//! Config schema targets **Open5GS 2.8.x** and **UERANSIM 3.2.x**. The
//! rendered config set has been proven **end to end** (2026-07-14) against
//! the containerized stack (`containers/fiveg/`, hybrid policy §15): the
//! full core (NRF/AUSF/UDM/UDR/PCF/BSF/NSSF/AMF + per-slice SMF/UPF) comes
//! up on rendered configs, the gNB completes **NG Setup**, and **both UEs
//! register and establish PDU sessions on their own S-NSSAI**, each getting
//! its slice's subnet (adas → 10.46.0.2 on uesimtun0, infotainment →
//! 10.47.0.2 on uesimtun1).
//!
//! [`lifecycle`] (2026-07-17) ports that proven `pod-up.sh` sequence into a
//! tested Rust module — pod up/down/status, idempotent. It is the
//! *mechanism* only: not yet wired to any vxAPI RPC or manifest verb.
//!
//! [`subscriber`] (2026-07-17) replaces `dbctl`-shelling with direct
//! `subscribers` document writes — sourced straight from the plan, never
//! re-parsed YAML — fixing a real bug found by reading `dbctl`'s own
//! source: its SD-less fallback (`add_ue_with_apn`) hardcoded `sst: 1`,
//! silently mis-registering any slice without an SD under the wrong SST.
//!
//! Remaining before H2 numbers: GatewayService wiring so `mode: s-nssai`
//! slices provision through the normal `vxedge up` lifecycle, and
//! uesimtun↔vxgateway bridging so measured traffic rides the PDU sessions.

pub mod lifecycle;
pub mod plan;
pub mod render;
pub mod subscriber;

pub use lifecycle::{
    FiveGLifecycleConfig, FiveGLifecycleError, FiveGStatus, fiveg_down, fiveg_status, fiveg_up,
};
pub use plan::{FiveGPlan, FiveGPlanError, SliceSession};
pub use render::{ConfigFile, render_all};
pub use subscriber::{subscriber_insert_commands, subscriber_insert_script};
