//! Bridges `vxedge_core::status::Observable` (domain-level, proto-agnostic) onto
//! `vxedge_api::v1::ResourceStatus` (the wire envelope) — the one place that translation
//! happens, replacing the ad hoc `if state == X { ResourceState::Up } else { ... }` conversion
//! every RPC handler used to write for itself.
//!
//! `vxedge-core` never depends on `vxedge-api` (`AGENTS.md`'s own crate-layering rule: the
//! domain model stays proto-agnostic), so this mapping can only live here, in the daemon that
//! already depends on both.

use vxedge_api::v1::{
    NodeBackend, NodePayload, NodeSpec, NodeStatus, PayloadKind, PayloadRun, QemuOptions,
    ResourceKind, ResourceState, ResourceStatus,
};
use vxedge_core::model::{Backend, DeviceRole};
use vxedge_core::status::{Observable, ObservedState};
use vxedge_core::zones::NodePlan;
use vxedge_core::zones::backend::NodeState;

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

/// The kind a device reports as: the role an OEM engineer thinks in, whichever service addressed it.
pub fn node_kind(role: vxedge_core::model::DeviceRole) -> ResourceKind {
    if role == vxedge_core::model::DeviceRole::Hpc {
        ResourceKind::Compute
    } else {
        ResourceKind::Zone
    }
}

/// The role as the manifest spells it.
fn role_name(role: DeviceRole) -> &'static str {
    match role {
        DeviceRole::ZoneController => "zone-controller",
        DeviceRole::Hpc => "hpc",
        DeviceRole::Gateway => "gateway",
        DeviceRole::Ecu => "ecu",
        DeviceRole::Sensor => "sensor",
        DeviceRole::Actuator => "actuator",
        DeviceRole::Switch => "switch",
    }
}

fn node_payload(np: &NodePlan) -> NodePayload {
    let part = |kind: PayloadKind, local: Option<&std::path::Path>| match &np.remote_bundle {
        Some((reference, digest)) => NodePayload {
            kind: kind as i32,
            reference: reference.clone(),
            digest: digest.clone(),
            run: None,
        },
        None => NodePayload {
            kind: kind as i32,
            reference: local.map(|p| p.display().to_string()).unwrap_or_default(),
            digest: String::new(),
            run: None,
        },
    };
    match np.backend {
        Backend::Container => match &np.image {
            Some(image) => {
                let (reference, digest) = image.split_once('@').unwrap_or((image, ""));
                NodePayload {
                    kind: PayloadKind::Container as i32,
                    reference: reference.to_string(),
                    digest: digest.to_string(),
                    run: (!np.run.is_empty()).then(|| PayloadRun {
                        args: np.run.args.clone(),
                        env: np.run.env.clone().into_iter().collect(),
                        cap_add: np.run.cap_add.clone(),
                    }),
                }
            }
            None => NodePayload {
                kind: PayloadKind::HostProcess as i32,
                ..Default::default()
            },
        },
        Backend::WasmCapsule => part(PayloadKind::Wasm, np.module.as_deref()),
        Backend::QemuMicrovm | Backend::Libvirt | Backend::CloudHypervisor | Backend::QemuMcu => {
            part(PayloadKind::GuestBundle, np.bundle.as_deref())
        }
        Backend::Attach => part(PayloadKind::AttachAdapter, np.bundle.as_deref()),
    }
}

/// The vxAPI form of a backend.
pub fn node_backend(backend: Backend) -> NodeBackend {
    match backend {
        Backend::Container => NodeBackend::Container,
        Backend::QemuMicrovm => NodeBackend::QemuMicrovm,
        Backend::Libvirt => NodeBackend::Libvirt,
        Backend::CloudHypervisor => NodeBackend::CloudHypervisor,
        Backend::WasmCapsule => NodeBackend::WasmCapsule,
        Backend::Attach => NodeBackend::Attach,
        Backend::QemuMcu => NodeBackend::QemuMcu,
    }
}

/// One node as the workspace plans it: the typed form of [`node_detail`].
pub fn node_spec(np: &NodePlan) -> NodeSpec {
    let backend = node_backend(np.backend);
    NodeSpec {
        vxid: np.node_id.clone(),
        zone: np.zone_id.clone(),
        role: role_name(np.role).to_string(),
        backend: backend as i32,
        port: np.endpoint.clone(),
        peer: np.peer.clone(),
        vlan: u32::from(np.vlan),
        slice: np.slice.clone().unwrap_or_default(),
        payload: Some(node_payload(np)),
        qemu: (np.backend == Backend::QemuMicrovm).then(|| QemuOptions {
            machine: np.qemu.machine.as_str().to_string(),
            transport: np.qemu.transport.as_str().to_string(),
            vhost: np.qemu.vhost,
        }),
    }
}

/// `peer` is here because interface names are derived from the device id and shortened to fit
/// IFNAMSIZ, so `vz0-node-a-c06a` is not something an operator reconstructs by eye. For a
/// container-backed node the useful half is the peer: that is the end campaign scripts address.
fn node_detail(np: &NodePlan) -> String {
    format!(
        "zone={} backend={:?} port={} peer={} vlan={}",
        np.zone_id, np.backend, np.endpoint, np.peer, np.vlan
    )
}

/// One node, observed. The one place that builds a node's status, so the text and the typed
/// fields cannot disagree.
pub fn observed_node(
    np: &NodePlan,
    state: &NodeState,
    platform: Option<vxedge_core::zones::GuestPlatform>,
) -> NodeStatus {
    NodeStatus {
        status: Some(resource_status(
            np.node_id.clone(),
            node_kind(np.role),
            state,
            node_detail(np),
        )),
        spec: Some(node_spec(np)),
        arch: platform
            .map(|p| p.arch.as_str().to_string())
            .unwrap_or_default(),
        acceleration: platform
            .map(|p| p.acceleration.as_str().to_string())
            .unwrap_or_default(),
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
        observed_unix_ms: now_ms(),
        ..Default::default()
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
                ResourceState::Down | ResourceState::Unspecified | ResourceState::Unprovisioned => {
                    ObservedState::Down
                }
                ResourceState::Staging => ObservedState::Staging,
                ResourceState::Degraded => ObservedState::Degraded,
                ResourceState::Error => ObservedState::Error,
            },
        )
        .collect();
    wire_state(vxedge_core::status::aggregate(&states))
}

/// Rolls a gateway status plus a set of already-built component statuses up into one parent
/// `ResourceStatus` (state aggregated via [`aggregate_resource_states`]) and the full
/// component list, gateway first. Shared by `ExperimentService::status` and
/// `VehicleService::status` — same composite-rollup logic, differing only in which proto
/// message (`ExperimentStatus`/`VehicleStatus`) and `ResourceKind` (`Experiment`/`Vehicle`)
/// wrap the result, which is the caller's job, not this function's.
pub fn composite_rollup(
    id: &str,
    kind: ResourceKind,
    gw_status: ResourceStatus,
    mut components: Vec<ResourceStatus>,
) -> (ResourceStatus, Vec<ResourceStatus>) {
    let mut resources = vec![ResourceStatus {
        vxid: "vxgateway".into(),
        kind: ResourceKind::Gateway as i32,
        state: gw_status.state,
        detail: gw_status.detail.clone(),
        since_unix_ms: gw_status.since_unix_ms,
        observed_unix_ms: gw_status.observed_unix_ms,
    }];
    resources.append(&mut components);
    let state = aggregate_resource_states(&resources);
    let top = ResourceStatus {
        vxid: id.to_string(),
        kind: kind as i32,
        state: state as i32,
        detail: format!("{} resource(s)", resources.len()),
        since_unix_ms: gw_status.since_unix_ms,
        observed_unix_ms: gw_status.observed_unix_ms,
    };
    (top, resources)
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
