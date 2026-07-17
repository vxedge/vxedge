//! TrafficPlan: desired-state compilation of a manifest's `traffic_profiles`
//! into concrete, slice-mode-aware endpoints. Pure data — no privileges, no
//! I/O — mirroring `GatewayPlan`/`FiveGPlan`.
//!
//! Resolves each profile's `source`/`target` (a `<zone-vxid>:<node-vxid>`
//! reference, or the symbolic `internet`) against the manifest's own
//! zone/node/slice declarations, and — for `mode: vlan` endpoints only —
//! against the *already-computed* `GatewayPlan.endpoints`, never
//! re-deriving peer-interface naming independently. `zone-backend-contract`
//! is explicit that `NodePlan`/`GatewayPlan` endpoints are joined by
//! identical traversal order, asserted, not assumed — this module reuses
//! that join instead of re-deriving a second, potentially-diverging one.
//! `mode: s-nssai` endpoints don't appear in `GatewayPlan.endpoints` at all
//! (built only for VLAN-bound ifaces), so they're resolved from the raw
//! manifest slice/node declaration alone; their real transport address
//! (the UE's PDU-session IP) isn't known until the lifecycle stage
//! discovers it live — this plan only records *which slice*.

use crate::gateway::GatewayPlan;
use crate::manifest::{ExperimentManifest, NodeIface, SliceMode, TrafficProfile};

#[derive(Debug, thiserror::Error)]
pub enum TrafficPlanError {
    #[error(
        "traffic_profiles.{profile}.{field} `{reference}` is not `<zone-vxid>:<node-vxid>` or `internet`"
    )]
    InvalidReference {
        profile: String,
        field: &'static str,
        reference: String,
    },
    #[error("traffic_profiles.{profile}.{field} references unknown zone/node `{reference}`")]
    UnknownNode {
        profile: String,
        field: &'static str,
        reference: String,
    },
    #[error(
        "traffic_profiles.{profile}.{field} node `{reference}` has no `network[].slice`-bound interface"
    )]
    NodeHasNoSlice {
        profile: String,
        field: &'static str,
        reference: String,
    },
    #[error(
        "traffic_profiles.{profile}.{field} node `{reference}` binds slice `{slice}`, which isn't declared in `network.slices`"
    )]
    UnknownSlice {
        profile: String,
        field: &'static str,
        reference: String,
        slice: String,
    },
    #[error(
        "traffic_profiles.{profile}.{field} node `{reference}` binds a `mode: vlan` slice but has no matching GatewayPlan endpoint — GatewayPlan and this TrafficPlan must be built from the same manifest"
    )]
    MissingGatewayEndpoint {
        profile: String,
        field: &'static str,
        reference: String,
    },
    #[error(
        "traffic_profiles.{0} has both source and target as symbolic references (`internet`/unresolvable) — at least one side must name a real zone/node so a slice can be derived"
    )]
    BothEndpointsSymbolic(String),
    #[error("traffic_profiles.{0} has neither `pps` nor `bandwidth_mbps` set")]
    NoRateSpecified(String),
    #[error(
        "vlan {0} can't be embedded in the `10.<vlan>.0.x` addressing convention (max 255) — \
         out of range for every experiment manifest today, but a real limit worth knowing about"
    )]
    VlanTooLargeForAddressing(u16),
    #[error(
        "traffic_profiles.{0} has real node references for both source and target on the same \
         vlan — the `10.<vlan>.0.x` convention only has a near (`.2`) and far (`.1`) slot, not \
         two node slots; pair a real node with the symbolic `internet` instead (H1's own \
         methodology never needs two real nodes on one profile)"
    )]
    BothEndpointsRealOnSameVlan(String),
}

/// VLAN-mode addressing convention: `10.<vlan>.0.2` (node-peer/"near" side),
/// `10.<vlan>.0.1` (uplink-far side) — exactly what `run-exp-001.sh`
/// hardcodes today (`10.101.0.2`/`10.101.0.1` for vlan 101), now derived
/// once, deterministically, in one place. Fully determined by the VLAN ID
/// alone, so it belongs in the pure plan layer, not the lifecycle stage —
/// same philosophy `GatewayPlan`/`FiveGPlan` already apply to their own
/// addressing. Caps VLAN IDs at 255: embedding the ID directly as an IPv4
/// octet can't represent the full 802.1Q range (up to 4094), but every
/// existing experiment manifest uses low VLAN IDs (101/103) and H1's own
/// convention already carries this same ceiling implicitly — documented
/// honestly here rather than silently wrapping/truncating a larger ID.
fn vlan_endpoint_addr(vlan: u16, far_side: bool) -> Result<String, TrafficPlanError> {
    if vlan > 255 {
        return Err(TrafficPlanError::VlanTooLargeForAddressing(vlan));
    }
    Ok(format!("10.{vlan}.0.{}", if far_side { 1 } else { 2 }))
}

/// One endpoint of a resolved profile — where the process actually binds
/// (source) or sends to (target), keyed by slice mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileEndpoint {
    /// `mode: vlan` — this zone node's own gateway-side veth peer, the same
    /// host-visible interface `apply_endpoints` creates regardless of
    /// whether `zone up` ever ran (H1's own runner measures here directly,
    /// not inside a live container — `docs/STATUS.md`'s H2-campaign entry).
    /// `addr` is this endpoint's own address on it (`vlan_endpoint_addr`).
    VlanNodePeer {
        peer_iface: String,
        vlan: u16,
        addr: String,
    },
    /// `mode: vlan`, symbolic `internet` (no specific node) — the gateway's
    /// own uplink peer, VLAN-tagged for the paired endpoint's slice: a
    /// subinterface the lifecycle stage creates itself (`<uplink_peer>.<vlan>`),
    /// exactly the convention `run-exp-001.sh` hand-assembles today.
    VlanUplinkFarSide {
        uplink_peer: String,
        vlan: u16,
        addr: String,
    },
    /// `mode: s-nssai` — this slice's UE. Its PDU-session address isn't
    /// known at plan time (assigned only once registration completes); the
    /// lifecycle stage resolves it live, the same way
    /// `fiveg::lifecycle::ue_probe_up` already does.
    SnssaiUe { slice_id: String },
    /// `mode: s-nssai`, symbolic `internet` (no specific node) — the CN
    /// pod's own bridge-attach address
    /// (`fiveg::lifecycle::BRIDGE_ATTACH_CN_ADDR`). Nothing needs to be
    /// listening there for a one-directional load profile (`vxtraffic`
    /// never expects a reply).
    SnssaiFarSide,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    pub id: String,
    pub source: ProfileEndpoint,
    pub target: ProfileEndpoint,
    pub pps: Option<u64>,
    pub bandwidth_mbps: Option<u64>,
    pub payload_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrafficPlan {
    pub profiles: Vec<ResolvedProfile>,
}

/// A reference resolved against the raw manifest, before deciding which
/// `ProfileEndpoint` variant it becomes — carries just enough to build
/// either a real or a symbolic endpoint once paired with the other side.
struct RealRef<'a> {
    node_id: &'a str,
    slice_id: &'a str,
    mode: SliceMode,
}

impl TrafficPlan {
    pub fn from_manifest(
        manifest: &ExperimentManifest,
        gateway_plan: &GatewayPlan,
    ) -> Result<Self, TrafficPlanError> {
        let mut profiles = Vec::with_capacity(manifest.traffic_profiles.len());
        for (id, profile) in &manifest.traffic_profiles {
            profiles.push(resolve_profile(manifest, gateway_plan, id, profile)?);
        }
        Ok(TrafficPlan { profiles })
    }
}

fn resolve_profile(
    manifest: &ExperimentManifest,
    gateway_plan: &GatewayPlan,
    id: &str,
    profile: &TrafficProfile,
) -> Result<ResolvedProfile, TrafficPlanError> {
    if profile.pps.is_none() && profile.bandwidth_mbps.is_none() {
        return Err(TrafficPlanError::NoRateSpecified(id.to_string()));
    }

    let source_ref = find_real_ref(manifest, id, "source", &profile.source)?;
    let target_ref = find_real_ref(manifest, id, "target", &profile.target)?;

    let (source, target) = match (source_ref, target_ref) {
        (Some(s), Some(t)) => {
            // The `10.<vlan>.0.x` addressing convention only has two slots
            // (near `.2`, far `.1`) — it has no way to give two *real* node
            // references on the *same* vlan distinct addresses without
            // colliding. H1's own methodology never needs this (every
            // profile pairs exactly one real node with the symbolic
            // `internet`); reject explicitly rather than silently handing
            // both endpoints the same address.
            if s.mode == SliceMode::Vlan && t.mode == SliceMode::Vlan {
                let source_vlan = gateway_plan
                    .endpoints
                    .iter()
                    .find(|ep| ep.node == s.node_id)
                    .map(|ep| ep.vlan);
                let target_vlan = gateway_plan
                    .endpoints
                    .iter()
                    .find(|ep| ep.node == t.node_id)
                    .map(|ep| ep.vlan);
                if source_vlan.is_some() && source_vlan == target_vlan {
                    return Err(TrafficPlanError::BothEndpointsRealOnSameVlan(
                        id.to_string(),
                    ));
                }
            }
            (
                endpoint_for(manifest, gateway_plan, id, "source", &profile.source, s)?,
                endpoint_for(manifest, gateway_plan, id, "target", &profile.target, t)?,
            )
        }
        (Some(s), None) => {
            let target = far_side_for(gateway_plan, id, "source", &profile.source, &s)?;
            let source = endpoint_for(manifest, gateway_plan, id, "source", &profile.source, s)?;
            (source, target)
        }
        (None, Some(t)) => {
            let source = far_side_for(gateway_plan, id, "target", &profile.target, &t)?;
            let target = endpoint_for(manifest, gateway_plan, id, "target", &profile.target, t)?;
            (source, target)
        }
        (None, None) => return Err(TrafficPlanError::BothEndpointsSymbolic(id.to_string())),
    };

    Ok(ResolvedProfile {
        id: id.to_string(),
        source,
        target,
        pps: profile.pps,
        bandwidth_mbps: profile.bandwidth_mbps,
        payload_bytes: profile.payload_bytes.unwrap_or(64),
    })
}

/// `reference` is either `<zone-vxid>:<node-vxid>` (resolved to `Some`) or a
/// symbolic string like `internet`/`carla` (resolved to `None` — not an
/// error by itself; only an error if *both* sides of a profile are
/// symbolic, checked by the caller).
fn find_real_ref<'a>(
    manifest: &'a ExperimentManifest,
    profile: &str,
    field: &'static str,
    reference: &'a str,
) -> Result<Option<RealRef<'a>>, TrafficPlanError> {
    let Some((zone_id, node_id)) = reference.split_once(':') else {
        return Ok(None);
    };
    let node = manifest
        .zones
        .iter()
        .find(|z| z.id == zone_id)
        .and_then(|z| z.nodes.iter().find(|n| n.id == node_id))
        .ok_or_else(|| TrafficPlanError::UnknownNode {
            profile: profile.to_string(),
            field,
            reference: reference.to_string(),
        })?;
    let iface: &NodeIface = node
        .network
        .iter()
        .find(|i| i.slice.is_some())
        .ok_or_else(|| TrafficPlanError::NodeHasNoSlice {
            profile: profile.to_string(),
            field,
            reference: reference.to_string(),
        })?;
    let slice_id = iface.slice.as_deref().expect("checked by find above");
    let slice = manifest
        .network
        .as_ref()
        .map(|n| n.slices.as_slice())
        .unwrap_or(&[])
        .iter()
        .find(|s| s.id == slice_id)
        .ok_or_else(|| TrafficPlanError::UnknownSlice {
            profile: profile.to_string(),
            field,
            reference: reference.to_string(),
            slice: slice_id.to_string(),
        })?;
    Ok(Some(RealRef {
        node_id,
        slice_id: &slice.id,
        mode: slice.mode,
    }))
}

fn endpoint_for(
    _manifest: &ExperimentManifest,
    gateway_plan: &GatewayPlan,
    profile: &str,
    field: &'static str,
    reference: &str,
    real: RealRef<'_>,
) -> Result<ProfileEndpoint, TrafficPlanError> {
    match real.mode {
        SliceMode::Vlan => {
            let ep = gateway_plan
                .endpoints
                .iter()
                .find(|ep| ep.node == real.node_id)
                .ok_or_else(|| TrafficPlanError::MissingGatewayEndpoint {
                    profile: profile.to_string(),
                    field,
                    reference: reference.to_string(),
                })?;
            Ok(ProfileEndpoint::VlanNodePeer {
                peer_iface: ep.peer.clone(),
                vlan: ep.vlan,
                addr: vlan_endpoint_addr(ep.vlan, false)?,
            })
        }
        SliceMode::Snssai => Ok(ProfileEndpoint::SnssaiUe {
            slice_id: real.slice_id.to_string(),
        }),
    }
}

/// The symbolic "far side" of whichever concrete endpoint this profile's
/// other reference resolved to — see `ProfileEndpoint`'s own doc comments
/// for what each variant means physically. For `mode: vlan`, the vlan
/// itself is looked up from the same `GatewayPlan.endpoint` the concrete
/// side resolves against — never re-derived from the manifest directly, to
/// stay joined to `GatewayPlan`'s own traversal order.
fn far_side_for(
    gateway_plan: &GatewayPlan,
    profile: &str,
    field: &'static str,
    reference: &str,
    real: &RealRef<'_>,
) -> Result<ProfileEndpoint, TrafficPlanError> {
    match real.mode {
        SliceMode::Vlan => {
            let ep = gateway_plan
                .endpoints
                .iter()
                .find(|ep| ep.node == real.node_id)
                .ok_or_else(|| TrafficPlanError::MissingGatewayEndpoint {
                    profile: profile.to_string(),
                    field,
                    reference: reference.to_string(),
                })?;
            Ok(ProfileEndpoint::VlanUplinkFarSide {
                uplink_peer: gateway_plan.uplink_peer.clone(),
                vlan: ep.vlan,
                addr: vlan_endpoint_addr(ep.vlan, true)?,
            })
        }
        SliceMode::Snssai => Ok(ProfileEndpoint::SnssaiFarSide),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::GatewayPlan;

    fn manifest(yaml: &str) -> ExperimentManifest {
        ExperimentManifest::from_yaml(yaml).expect("valid test manifest")
    }

    const BASE: &str = r#"
experiment: { id: exp-t, name: t }
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }
    - { id: infotainment, mode: s-nssai, s_nssai: { sst: 2 } }
zones:
  - id: vxzone-fl-01
    role: front-left
    nodes:
      - id: node-adas-01
        type: ecu
        image: { url: x, kind: container }
        backend: container
        network: [{ iface: eth0, vlan: 101, slice: adas }]
      - id: node-info-01
        type: infotainment
        image: { url: x, kind: container }
        backend: container
        network: [{ iface: eth0, slice: infotainment }]
traffic_profiles:
  adas-probe:
    source: vxzone-fl-01:node-adas-01
    target: internet
    pps: 1000
  infotainment-load:
    source: vxzone-fl-01:node-info-01
    target: internet
    bandwidth_mbps: 400
"#;

    fn plans(yaml: &str) -> (ExperimentManifest, GatewayPlan) {
        let m = manifest(yaml);
        let gw = GatewayPlan::from_manifest(&m).expect("gateway plans");
        (m, gw)
    }

    #[test]
    fn vlan_profile_resolves_to_node_peer_and_uplink_far_side() {
        let (m, gw) = plans(BASE);
        let plan = TrafficPlan::from_manifest(&m, &gw).expect("traffic plans");
        let p = plan
            .profiles
            .iter()
            .find(|p| p.id == "adas-probe")
            .expect("adas-probe present");
        assert_eq!(
            p.source,
            ProfileEndpoint::VlanNodePeer {
                peer_iface: "vzp0".to_string(),
                vlan: 101,
                addr: "10.101.0.2".to_string(),
            }
        );
        assert_eq!(
            p.target,
            ProfileEndpoint::VlanUplinkFarSide {
                uplink_peer: gw.uplink_peer.clone(),
                vlan: 101,
                addr: "10.101.0.1".to_string(),
            }
        );
        assert_eq!(p.pps, Some(1000));
        assert_eq!(p.payload_bytes, 64, "defaults to 64 when unset");
    }

    #[test]
    fn snssai_profile_resolves_to_ue_and_far_side() {
        let (m, gw) = plans(BASE);
        let plan = TrafficPlan::from_manifest(&m, &gw).expect("traffic plans");
        let p = plan
            .profiles
            .iter()
            .find(|p| p.id == "infotainment-load")
            .expect("infotainment-load present");
        assert_eq!(
            p.source,
            ProfileEndpoint::SnssaiUe {
                slice_id: "infotainment".to_string(),
            }
        );
        assert_eq!(p.target, ProfileEndpoint::SnssaiFarSide);
        assert_eq!(p.bandwidth_mbps, Some(400));
    }

    #[test]
    fn rejects_two_real_nodes_on_the_same_vlan() {
        // The `10.<vlan>.0.x` convention only has a near (`.2`) and far
        // (`.1`) slot; two real nodes on the *same* vlan have no distinct
        // slot to land on. H1's own methodology never needs this (every
        // profile pairs one real node with the symbolic `internet`).
        let yaml = r#"
experiment: { id: exp-t, name: t }
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
zones:
  - id: vxzone-fl-01
    role: front-left
    nodes:
      - id: node-a
        type: ecu
        image: { url: x, kind: container }
        backend: container
        network: [{ iface: eth0, vlan: 101, slice: adas }]
      - id: node-b
        type: ecu
        image: { url: x, kind: container }
        backend: container
        network: [{ iface: eth0, vlan: 101, slice: adas }]
traffic_profiles:
  node-to-node:
    source: vxzone-fl-01:node-a
    target: vxzone-fl-01:node-b
    pps: 500
"#;
        let (m, gw) = plans(yaml);
        let err = TrafficPlan::from_manifest(&m, &gw).unwrap_err();
        assert!(matches!(
            err,
            TrafficPlanError::BothEndpointsRealOnSameVlan(id) if id == "node-to-node"
        ));
    }

    #[test]
    fn two_real_nodes_on_different_vlans_resolve_independently() {
        // Different vlans embed distinct addresses, so no collision — this
        // is a legitimate, supported "both real" case.
        let yaml = r#"
experiment: { id: exp-t, name: t }
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
    - { id: infotainment, mode: vlan, vlan: 103 }
zones:
  - id: vxzone-fl-01
    role: front-left
    nodes:
      - id: node-a
        type: ecu
        image: { url: x, kind: container }
        backend: container
        network: [{ iface: eth0, vlan: 101, slice: adas }]
      - id: node-b
        type: ecu
        image: { url: x, kind: container }
        backend: container
        network: [{ iface: eth0, vlan: 103, slice: infotainment }]
traffic_profiles:
  cross-slice:
    source: vxzone-fl-01:node-a
    target: vxzone-fl-01:node-b
    pps: 500
"#;
        let (m, gw) = plans(yaml);
        let plan = TrafficPlan::from_manifest(&m, &gw).expect("traffic plans");
        let p = &plan.profiles[0];
        assert_eq!(
            p.source,
            ProfileEndpoint::VlanNodePeer {
                peer_iface: "vzp0".to_string(),
                vlan: 101,
                addr: "10.101.0.2".to_string(),
            }
        );
        assert_eq!(
            p.target,
            ProfileEndpoint::VlanNodePeer {
                peer_iface: "vzp1".to_string(),
                vlan: 103,
                addr: "10.103.0.2".to_string(),
            }
        );
    }

    #[test]
    fn rejects_unknown_node_reference() {
        let (m, gw) = plans(BASE);
        let mut m = m;
        m.traffic_profiles.get_mut("adas-probe").unwrap().source = "no-such-zone:no-node".into();
        let err = TrafficPlan::from_manifest(&m, &gw).unwrap_err();
        assert!(matches!(err, TrafficPlanError::UnknownNode { .. }));
    }

    #[test]
    fn rejects_profile_with_no_rate() {
        let (m, gw) = plans(BASE);
        let mut m = m;
        m.traffic_profiles.get_mut("adas-probe").unwrap().pps = None;
        let err = TrafficPlan::from_manifest(&m, &gw).unwrap_err();
        assert!(matches!(err, TrafficPlanError::NoRateSpecified(id) if id == "adas-probe"));
    }

    #[test]
    fn rejects_both_endpoints_symbolic() {
        let (m, gw) = plans(BASE);
        let mut m = m;
        m.traffic_profiles.get_mut("adas-probe").unwrap().source = "internet".into();
        let err = TrafficPlan::from_manifest(&m, &gw).unwrap_err();
        assert!(matches!(err, TrafficPlanError::BothEndpointsSymbolic(id) if id == "adas-probe"));
    }

    #[test]
    fn rejects_node_with_no_slice_bound_interface() {
        let yaml = r#"
experiment: { id: exp-t, name: t }
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
zones:
  - id: vxzone-fl-01
    role: front-left
    nodes:
      - id: node-a
        type: ecu
        image: { url: x, kind: container }
        backend: container
        network: [{ iface: eth0 }]
traffic_profiles:
  bad:
    source: vxzone-fl-01:node-a
    target: internet
    pps: 100
"#;
        let (m, gw) = plans(yaml);
        let err = TrafficPlan::from_manifest(&m, &gw).unwrap_err();
        assert!(matches!(err, TrafficPlanError::NodeHasNoSlice { .. }));
    }

    #[test]
    fn rejects_vlan_too_large_for_addressing() {
        let yaml = r#"
experiment: { id: exp-t, name: t }
network:
  slices:
    - { id: adas, mode: vlan, vlan: 300 }
zones:
  - id: vxzone-fl-01
    role: front-left
    nodes:
      - id: node-a
        type: ecu
        image: { url: x, kind: container }
        backend: container
        network: [{ iface: eth0, vlan: 300, slice: adas }]
traffic_profiles:
  probe:
    source: vxzone-fl-01:node-a
    target: internet
    pps: 100
"#;
        let (m, gw) = plans(yaml);
        let err = TrafficPlan::from_manifest(&m, &gw).unwrap_err();
        assert!(matches!(
            err,
            TrafficPlanError::VlanTooLargeForAddressing(300)
        ));
    }
}
