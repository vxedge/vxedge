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
//! tested Rust module — pod up/down/status, idempotent — and is now wired
//! into `GatewayService` (`vxedged`): `vxedge up/down/status` provision the
//! CN/RAN pods and attach the CN pod to the real `vxbr0` whenever a
//! manifest declares `mode: s-nssai` slices.
//!
//! [`subscriber`] (2026-07-17) replaces `dbctl`-shelling with direct
//! `subscribers` document writes — sourced straight from the plan, never
//! re-parsed YAML — fixing a real bug found by reading `dbctl`'s own
//! source: its SD-less fallback (`add_ue_with_apn`) hardcoded `sst: 1`,
//! silently mis-registering any slice without an SD under the wrong SST.
//!
//! [`lifecycle::ue_up`]/[`lifecycle::ue_down`] (2026-07-17) give each UE its
//! own pod/netns, joined to gNB via a bridge internal to the RAN pod —
//! fixes a real bug found live during the bridging proof: UERANSIM
//! hardcodes routing table `1000` for its per-UE default route, so UEs
//! sharing one netns collide on it. Still started separately per test run,
//! not by `fiveg_up` (the vxprobe/vxtraffic pattern).
//!
//! Remaining before H2 numbers: URSP-style `slice_policy`, image-digest
//! recording into results metadata.

pub mod lifecycle;
pub mod plan;
pub mod render;
pub mod subscriber;

pub use lifecycle::{
    FIVEG_UPLINK_VLAN, FiveGLifecycleConfig, FiveGLifecycleError, FiveGStatus, bridge_down,
    bridge_host_veth, bridge_up, fiveg_down, fiveg_status, fiveg_up, pod_exists, pod_pid, ue_down,
    ue_pod_name, ue_probe_up, ue_up,
};
pub use plan::{FiveGPlan, FiveGPlanError, SliceSession};
pub use render::{ConfigFile, render_all};
pub use subscriber::{subscriber_insert_commands, subscriber_insert_script};
