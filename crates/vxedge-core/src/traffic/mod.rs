//! TrafficService backend: compiles `traffic_profiles` into a resolved plan
//! ([`TrafficPlan`], mode-aware over `vlan`/`s-nssai` slices, mirroring
//! `GatewayPlan`/`FiveGPlan`'s own shape), then drives real `vxprobe`/
//! `vxtraffic` processes against it ([`lifecycle`]) — the real replacement
//! for what `vxperiments/scripts/run-exp-001.sh` currently hand-assembles
//! with raw `ip addr`/`ip link add ... type vlan` and hardcoded addresses.

pub mod plan;

pub use plan::{ProfileEndpoint, ResolvedProfile, TrafficPlan, TrafficPlanError};
