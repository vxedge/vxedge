//! Bridges `vxedge_core::status::Observable` (domain-level, proto-agnostic) onto
//! `vxedge_api::v1::ResourceStatus` (the wire envelope) — the one place that translation
//! happens, replacing the ad hoc `if state == X { ResourceState::Up } else { ... }` conversion
//! every RPC handler used to write for itself.
//!
//! `vxedge-core` never depends on `vxedge-api` (`AGENTS.md`'s own crate-layering rule: the
//! domain model stays proto-agnostic), so this mapping can only live here, in the daemon that
//! already depends on both.

use vxedge_api::v1::{ResourceKind, ResourceState, ResourceStatus};
use vxedge_core::status::{Observable, ObservedState};

use crate::jobs::now_ms;

fn wire_state(s: ObservedState) -> ResourceState {
    match s {
        ObservedState::Up => ResourceState::Up,
        ObservedState::Down => ResourceState::Down,
        ObservedState::Staging => ResourceState::Staging,
        ObservedState::Degraded => ResourceState::Degraded,
        ObservedState::Error => ResourceState::Error,
    }
}

/// `vxedge_core::model::TunnelKind` (domain) → `vxedge_api::v1::TunnelKind` (wire) — the same
/// one-place translation this module already owns for `ObservedState`/`ResourceState`.
pub fn wire_tunnel_kind(k: vxedge_core::model::TunnelKind) -> vxedge_api::v1::TunnelKind {
    match k {
        vxedge_core::model::TunnelKind::Wireguard => vxedge_api::v1::TunnelKind::Wireguard,
        vxedge_core::model::TunnelKind::Vxlan => vxedge_api::v1::TunnelKind::Vxlan,
    }
}

/// Builds one `ResourceStatus` from anything `Observable`, plus the identity/kind vxedged
/// itself always knows. An `Observable` domain type never carries its own `vxid`/`kind` — those
/// are routing information the daemon owns (which node, which resource kind), not something
/// the observation itself has any way to know.
pub fn resource_status(
    vxid: impl Into<String>,
    kind: ResourceKind,
    o: &impl Observable,
    detail: impl Into<String>,
) -> ResourceStatus {
    ResourceStatus {
        vxid: vxid.into(),
        kind: kind as i32,
        state: wire_state(o.state()) as i32,
        detail: detail.into(),
        since_unix_ms: now_ms(),
    }
}

/// Rolls up a set of already-built `ResourceStatus`es into one parent state, via
/// `vxedge_core::status::aggregate`'s own severity rule. Wire-level convenience: callers
/// already holding `Vec<ResourceStatus>` (`ExperimentSvc::status`'s `components`) shouldn't
/// have to unwrap each one back into `ObservedState` by hand first.
pub fn aggregate_resource_states(statuses: &[ResourceStatus]) -> ResourceState {
    let states: Vec<ObservedState> = statuses
        .iter()
        .map(
            |s| match ResourceState::try_from(s.state).unwrap_or(ResourceState::Unspecified) {
                ResourceState::Up => ObservedState::Up,
                ResourceState::Down | ResourceState::Unspecified => ObservedState::Down,
                ResourceState::Staging => ObservedState::Staging,
                ResourceState::Degraded => ObservedState::Degraded,
                ResourceState::Error => ObservedState::Error,
            },
        )
        .collect();
    wire_state(vxedge_core::status::aggregate(&states))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vxedge_core::status::ObservedState;

    struct Fake(ObservedState);
    impl Observable for Fake {
        fn state(&self) -> ObservedState {
            self.0
        }
    }

    #[test]
    fn resource_status_carries_the_supplied_identity_and_maps_state() {
        let s = resource_status("node-a", ResourceKind::Zone, &Fake(ObservedState::Up), "up");
        assert_eq!(s.vxid, "node-a");
        assert_eq!(s.kind, ResourceKind::Zone as i32);
        assert_eq!(s.state, ResourceState::Up as i32);
        assert_eq!(s.detail, "up");
    }

    #[test]
    fn aggregate_resource_states_matches_the_domain_rule() {
        let up = resource_status("a", ResourceKind::Zone, &Fake(ObservedState::Up), "");
        let down = resource_status("b", ResourceKind::Zone, &Fake(ObservedState::Down), "");
        assert_eq!(
            aggregate_resource_states(&[up, down]),
            ResourceState::Degraded
        );
    }

    #[test]
    fn aggregate_resource_states_of_nothing_is_down() {
        assert_eq!(aggregate_resource_states(&[]), ResourceState::Down);
    }

    #[test]
    fn wire_state_maps_every_observed_state_distinctly() {
        // Every arm, explicitly — a future accidental swap (e.g. Staging <-> Error) would
        // otherwise go uncaught, since the other tests here only ever exercise Up/Down/
        // Degraded.
        assert_eq!(wire_state(ObservedState::Up), ResourceState::Up);
        assert_eq!(wire_state(ObservedState::Down), ResourceState::Down);
        assert_eq!(wire_state(ObservedState::Staging), ResourceState::Staging);
        assert_eq!(wire_state(ObservedState::Degraded), ResourceState::Degraded);
        assert_eq!(wire_state(ObservedState::Error), ResourceState::Error);
    }

    #[test]
    fn aggregate_resource_states_round_trips_staging_and_error() {
        // The reverse (wire ResourceState -> ObservedState) mapping inside
        // aggregate_resource_states, exercised for the two states the other tests never
        // touch: a lone Staging resource stays Staging, and Error still outranks Degraded in
        // a mix (matches vxedge_core::status::aggregate's own severity rule).
        let staging = resource_status("a", ResourceKind::Zone, &Fake(ObservedState::Staging), "");
        assert_eq!(
            aggregate_resource_states(&[staging]),
            ResourceState::Staging
        );

        let degraded = resource_status("b", ResourceKind::Zone, &Fake(ObservedState::Degraded), "");
        let error = resource_status("c", ResourceKind::Zone, &Fake(ObservedState::Error), "");
        assert_eq!(
            aggregate_resource_states(&[degraded, error]),
            ResourceState::Error
        );
    }
}
