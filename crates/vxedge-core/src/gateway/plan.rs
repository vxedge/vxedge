//! GatewayPlan: the desired-state compilation of a manifest's network section.
//! Pure data — no privileges needed — so `Apply` (plan/diff) and unit tests
//! run anywhere. Interface names are deterministic per manifest ordering,
//! which is what lets the reconciler match observed state on re-runs.

/// Linux IFNAMSIZ is 16 incl. NUL; our generated names stay well below it.
const MAX_SLICES: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(
        "device `{0}` is attached to a slice but declares no `backend` — the planner has to know \
         whether to build a veth (container) or a tap (qemu-microvm/libvirt), and guessing is \
         what produced a manifest that validated and then could not boot. Add \
         `backend: container|qemu-microvm|libvirt`."
    )]
    DeviceWithoutBackend(String),
    #[error("no `network.slices` at all — nothing to provision")]
    NoSlices,
    #[error(
        "attachments {a} and {b} both derive the host interface name `{name}`, so one would \
         provision over the other. Names come from the device id alone (see \
         `gateway::ifnames`), lowercased, punctuation-folded and shortened to fit the kernel's \
         15-byte limit — ids differing only in case or punctuation collapse to one name, and a \
         shortened pair can collide outright. Rename one of the two devices."
    )]
    EndpointNameCollision { a: String, b: String, name: String },
    #[error("too many slices: {0} (max {MAX_SLICES})")]
    TooManySlices(usize),
    #[error(
        "slice `{0}` declares `vlan: {1}`, which is reserved for the 5G core's own uplink \
         attachment (`fiveg::FIVEG_UPLINK_VLAN`) — pick a different VLAN id"
    )]
    VlanCollidesWithFiveGUplink(String, u16),
    #[error(
        "link `{link}` declares `impairments` but has no attachment this gateway provisions, so \
         there is no access port to render netem onto. A link earns one by binding a `slice:` \
         and attaching a device that declares a `backend:`."
    )]
    ImpairmentNotRenderable { link: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlicePlan {
    pub id: String,
    pub vlan: u16,
    /// Guaranteed rate. HTB requires a non-zero rate; 0 in the manifest maps
    /// to a 1 mbit floor (best-effort slice).
    pub min_rate_mbps: u64,
    pub max_rate_mbps: u64,
    pub priority: u8,
    /// HTB class minor under parent 1:1 (hex 0x10, 0x11, ...).
    pub class_minor: u16,
}

/// How a node attaches to its bridge access port. Resolved from the node's
/// `backend:` (explicit) or its image kind: containers get a veth pair whose
/// peer moves into the node's namespace; VMs (libvirt / qemu-microvm) get a
/// tap the hypervisor opens directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    Veth,
    Tap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointPlan {
    /// Bridge access port (veth host side, or the tap itself).
    pub name: String,
    /// Veth peer where the node attaches (== `name` for taps).
    pub peer: String,
    pub kind: EndpointKind,
    /// vxid of the node this endpoint serves.
    pub node: String,
    pub vlan: u16,
    pub slice: Option<String>,
    /// The harness impairment this access port renders, when it is the one its link elected to
    /// carry it. See [`GatewayPlan::from_model`] for why exactly one endpoint per link does.
    pub impairment: Option<EndpointImpairment>,
}

/// A link's impairment, resolved onto the single access port that will render it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointImpairment {
    /// The link this came from. Carried so `status` and any results caveat can say *which*
    /// harness segment a netem represents, rather than only which interface it sits on.
    pub link: String,
    /// The IFB device carrying the ingress direction ([`crate::gateway::ifnames::ingress_ifb`]).
    pub ifb: String,
    pub params: crate::model::Impairment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayPlan {
    pub bridge: String,
    /// Trunk uplink (bridge side); the HTB queue tree lives on its egress —
    /// the shared bottleneck where slice contention is measured.
    pub uplink: String,
    /// Peer end of the uplink; traffic sinks/cloud side attach here.
    pub uplink_peer: String,
    /// Aggregate root rate for the HTB tree (from network.defaults.bandwidth,
    /// else 1000 mbit).
    pub root_rate_mbps: u64,
    pub slices: Vec<SlicePlan>,
    pub endpoints: Vec<EndpointPlan>,
    /// Ids of `mode: s-nssai` slices present in the manifest — tracked, not
    /// yet provisioned by this reconciler. Real S-NSSAI slice lifecycle is
    /// `vxedge_core::fiveg`'s job (its own CN/RAN pods); a manifest mixing
    /// vlan and s-nssai slices no longer fails outright here (previously
    /// `PlanError::SnssaiNotSupported`, removed 2026-07-17) — apply/
    /// teardown/observe below are still vlan-slice-only and untouched, so
    /// this is additive: existing vlan-only plans are unaffected byte for
    /// byte. Bridging an s-nssai slice's fiveg pod into `bridge` as an
    /// access port is `fiveg`'s job, tracked in `vxedge/TASKS.md` §4.
    pub snssai_slice_ids: Vec<String>,
    /// Which mechanism renders this plan (`gateway::provision::data_plane`'s selector). Carried
    /// on the plan, not passed alongside it, because the choice is part of the provisioning
    /// intent itself — the same reason `slices`/`endpoints` live here rather than being derived
    /// again at each call site.
    pub data_plane: crate::model::DataPlaneKind,
}

/// One half of a collision message: which attachment a name was derived from.
fn describe_attachment(device: &str, port: Option<&str>) -> String {
    match port {
        Some(p) => format!("device `{device}` port `{p}`"),
        None => format!("device `{device}`"),
    }
}

/// A slice as the planner needs it, flattened out of the model's optional QoS block so the
/// defaulting happens in one place rather than at each use.
struct SliceInput {
    id: String,
    is_vlan: bool,
    vlan: Option<u16>,
    min_rate_mbps: u64,
    max_rate_mbps: u64,
    priority: u8,
}

/// Slice planning: HTB class ids, minor numbers and default rates, decided once.
fn plan_slices(slices: &[SliceInput]) -> Result<(Vec<SlicePlan>, Vec<String>), PlanError> {
    if slices.len() > MAX_SLICES {
        return Err(PlanError::TooManySlices(slices.len()));
    }
    let vlan_slices: Vec<SlicePlan> = slices
        .iter()
        .filter(|s| s.is_vlan)
        .enumerate()
        .map(|(i, s)| {
            let vlan = s.vlan.expect("validated: vlan slice has vlan id");
            if vlan == crate::fiveg::FIVEG_UPLINK_VLAN {
                return Err(PlanError::VlanCollidesWithFiveGUplink(s.id.clone(), vlan));
            }
            Ok(SlicePlan {
                id: s.id.clone(),
                vlan,
                min_rate_mbps: s.min_rate_mbps.max(1),
                max_rate_mbps: s.max_rate_mbps.max(1),
                priority: s.priority,
                class_minor: 0x10 + i as u16,
            })
        })
        .collect::<Result<_, PlanError>>()?;
    let snssai: Vec<String> = slices
        .iter()
        .filter(|s| !s.is_vlan)
        .map(|s| s.id.clone())
        .collect();
    if vlan_slices.is_empty() && snssai.is_empty() {
        return Err(PlanError::NoSlices);
    }
    Ok((vlan_slices, snssai))
}

/// Elect, per impaired link, the one access port that renders its netem.
///
/// # Why one port and not every port
///
/// A qdisc shapes what leaves an interface, so the obvious reading — impair every attachment of
/// the link — impairs each traversal *twice* whenever both ends are devices on this bridge. A
/// packet from `gw1` to `zonefl` leaves `gw1`, is impaired inbound at `vz-gw1`, crosses the
/// bridge, and is impaired again outbound at `vz-zonefl`: 500 µs of delay on a 250 µs wire, with
/// nothing anywhere reporting a problem. Halving the values to compensate would then be wrong
/// for the single-attachment links every frozen experiment actually declares.
///
/// So exactly one port carries it, impairing **both directions** there
/// ([`crate::gateway::provision`] renders the ingress half through an IFB). Each traversal pays
/// the declared cost once, in either direction, whether the link has one device attachment or
/// two.
///
/// The port is elected by lowest interface name, which is a pure function of the attachment
/// ([`crate::gateway::ifnames`]) — the same property that keeps traversal order out of every
/// other name here. Which end holds the netem is not observable in the measurement, so the rule
/// only has to be stable, and this one cannot be moved by editing an unrelated part of the file.
///
/// # No collision check, and why none is needed
///
/// Two links cannot elect the same port. Each entry of `endpoints` is produced by exactly one
/// attachment of exactly one link, so distinct link ids select disjoint index sets. The case
/// that looks like it might collide — one device attached to two links on the same port — is
/// already refused upstream as [`PlanError::EndpointNameCollision`], because those two
/// attachments derive one interface name.
fn assign_impairments(
    exp: &crate::model::Experiment,
    endpoints: &mut [EndpointPlan],
    endpoint_link: &[String],
) -> Result<(), PlanError> {
    for link in exp.entities.iter().flat_map(|e| e.links.iter()) {
        let Some(params) = link.impairments.filter(|i| !i.is_empty()) else {
            continue;
        };
        let carrier = endpoint_link
            .iter()
            .enumerate()
            .filter(|(_, id)| *id == &link.id)
            .map(|(i, _)| i)
            .min_by(|a, b| endpoints[*a].name.cmp(&endpoints[*b].name));
        // A link with impairments and no provisioned attachment renders nothing. The workspace
        // validator catches the common cause (no `slice:`) against the authored file; this
        // catches every cause, including a device the planner skipped, and refuses rather than
        // dropping the block.
        let Some(carrier) = carrier else {
            return Err(PlanError::ImpairmentNotRenderable {
                link: link.id.clone(),
            });
        };
        let ep = &mut endpoints[carrier];
        ep.impairment = Some(EndpointImpairment {
            link: link.id.clone(),
            ifb: super::ifnames::ingress_ifb(&ep.name),
            params,
        });
    }
    Ok(())
}

impl GatewayPlan {
    /// Plan from the domain model — the only planning path there is.
    ///
    /// **Endpoint names come from the device id**, not from position — see
    /// [`crate::gateway::ifnames`]. Traversal order therefore decides only the order of this
    /// vector, never what anything is called. That is what let the retired §6.1 schema
    /// (declaration order) and a §6.0 workspace (map order) describe the same topology and
    /// plan the same interfaces, which is how every migration was signed off.
    pub fn from_model(exp: &crate::model::Experiment) -> Result<Self, PlanError> {
        let flat: Vec<SliceInput> = exp
            .slices
            .iter()
            .map(|s| {
                let q = s.qos.clone().unwrap_or_default();
                SliceInput {
                    id: s.id.clone(),
                    is_vlan: s.mode == crate::model::SliceMode::Vlan,
                    vlan: s.vlan,
                    min_rate_mbps: u64::from(q.min_rate_mbps.unwrap_or(0)),
                    max_rate_mbps: u64::from(q.max_rate_mbps.unwrap_or(0)),
                    priority: q.priority.unwrap_or(4),
                }
            })
            .collect();
        let (slices, snssai_slice_ids) = plan_slices(&flat)?;

        let vlan_of = |slice: &str| -> Option<u16> {
            exp.slices
                .iter()
                .find(|s| s.id == slice)
                .and_then(|s| s.vlan)
        };

        let mut endpoints: Vec<EndpointPlan> = Vec::new();
        // Link id per entry of `endpoints`, same index. Parallel to the vector rather than a
        // field on it because it is scaffolding for one pass, not part of the plan.
        let mut endpoint_link: Vec<String> = Vec::new();
        // What each name was derived from, so a collision names both halves rather than
        // reporting that some interface exists twice.
        let mut claimed: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for entity in &exp.entities {
            for device in entity.devices.values() {
                for (link, attachment) in entity.links_for(&device.id) {
                    let Some(slice) = &link.slice else { continue };
                    let Some(vlan) = vlan_of(slice) else { continue };
                    // Guessing how a device is realised is what produced a manifest that
                    // validated and then could not boot (exp-001 migration, 2026-07-30). Say so
                    // instead.
                    let backend = device
                        .backend
                        .ok_or_else(|| PlanError::DeviceWithoutBackend(device.id.clone()))?;
                    let kind = match backend {
                        crate::model::Backend::Container => EndpointKind::Veth,
                        crate::model::Backend::Libvirt | crate::model::Backend::QemuMicrovm => {
                            EndpointKind::Tap
                        }
                    };
                    let port = attachment.port.as_deref();
                    let name = super::ifnames::access_port(&device.id, port);
                    let origin = describe_attachment(&device.id, port);
                    if let Some(other) = claimed.insert(name.clone(), origin.clone()) {
                        return Err(PlanError::EndpointNameCollision {
                            a: other,
                            b: origin,
                            name,
                        });
                    }
                    endpoints.push(EndpointPlan {
                        peer: match kind {
                            EndpointKind::Veth => super::ifnames::veth_peer(&device.id, port),
                            EndpointKind::Tap => name.clone(),
                        },
                        name,
                        kind,
                        node: device.id.clone(),
                        vlan,
                        slice: Some(slice.clone()),
                        impairment: None,
                    });
                    // Recorded for the impairment pass below, which needs to know which
                    // endpoints a given link produced — the loop above is keyed by device.
                    endpoint_link.push(link.id.clone());
                }
            }
        }

        assign_impairments(exp, &mut endpoints, &endpoint_link)?;

        Ok(Self {
            bridge: "vxbr0".into(),
            uplink: "vxup0".into(),
            uplink_peer: "vxup0p".into(),
            root_rate_mbps: exp
                .uplink_bandwidth_bps
                .map(|bps| (bps / 1_000_000).max(1))
                .unwrap_or(1000),
            slices,
            endpoints,
            snssai_slice_ids,
            data_plane: exp.data_plane,
        })
    }

    /// Human-readable plan summary (Apply output, logs).
    pub fn summary(&self) -> String {
        let slices = self
            .slices
            .iter()
            .map(|s| {
                format!(
                    "{} (vlan {}, {}..{} mbit, prio {})",
                    s.id, s.vlan, s.min_rate_mbps, s.max_rate_mbps, s.priority
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let snssai = if self.snssai_slice_ids.is_empty() {
            String::new()
        } else {
            format!(
                "; s-nssai slices (fiveg-managed, not provisioned here): [{}]",
                self.snssai_slice_ids.join(", ")
            )
        };
        // Impairments are named in the summary rather than left to `status`: they change what
        // every number in the run means, so "which harness am I measuring" has to be answerable
        // from the plan the operator approved.
        let impaired: Vec<&str> = self
            .endpoints
            .iter()
            .filter_map(|e| e.impairment.as_ref())
            .map(|i| i.link.as_str())
            .collect();
        let harness = if impaired.is_empty() {
            String::new()
        } else {
            format!("; impaired link(s): [{}]", impaired.join(", "))
        };
        format!(
            "bridge {} + uplink {} @ {} mbit; slices: [{slices}]{snssai}; {} endpoint(s){harness}",
            self.bridge,
            self.uplink,
            self.root_rate_mbps,
            self.endpoints.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §6.0 workspace text → checked model, the one way anything reaches a planner. Written as
    /// `---`-separated documents so a fixture is one string rather than a temp directory.
    fn model(yaml: &str) -> crate::model::Experiment {
        crate::model::load_yaml(yaml).expect("valid test workspace")
    }

    const BASE: &str = r#"
kind: experiment
id: exp-t
vehicles: [veh]
baseline: bl
network:
  defaults: { bandwidth: 500000000 }
  slices:
    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }
    - { id: infotainment, mode: vlan, vlan: 103, qos: { min_rate_mbps: 0, max_rate_mbps: 400, priority: 5 } }
---
kind: vehicle
id: veh
zones: { front-left: {} }
devices:
  node-ecu-01:  { role: ecu, zone: front-left, backend: libvirt }
  node-info-01: { role: ecu, zone: front-left, backend: libvirt }
functions:
  fn-ecu:  { deployed_on: node-ecu-01 }
  fn-info: { deployed_on: node-info-01 }
links:
  - { id: l-adas,  endpoints: [node-ecu-01:eth0],  slice: adas }
  - { id: l-infot, endpoints: [node-info-01:eth0], slice: infotainment }
---
kind: baseline
id: bl
parts:
  - { ref: ./bundle, target_ids: [fn-ecu, fn-info] }
"#;

    /// A workspace with slices and no devices — enough to plan a bridge, which is what the
    /// slice-only cases below are about.
    fn slices_only(slices: &str) -> String {
        format!(
            "kind: experiment\nid: exp-t\nvehicles: [veh]\nnetwork:\n  slices:\n{slices}\n---\nkind: vehicle\nid: veh\ndevices: {{}}\n"
        )
    }

    #[test]
    fn builds_deterministic_plan() {
        let plan = GatewayPlan::from_model(&model(BASE)).unwrap();
        assert_eq!(plan.bridge, "vxbr0");
        assert_eq!(plan.root_rate_mbps, 500);
        assert_eq!(plan.slices.len(), 2);
        assert_eq!(plan.slices[0].vlan, 101);
        assert_eq!(plan.slices[0].class_minor, 0x10);
        assert_eq!(plan.slices[1].class_minor, 0x11);
        // min rate 0 floors to 1 mbit (HTB needs non-zero rates).
        assert_eq!(plan.slices[1].min_rate_mbps, 1);
        assert_eq!(plan.endpoints.len(), 2);
        assert_eq!(plan.endpoints[0].name, "vz-node-ecu-01");
        assert_eq!(plan.endpoints[0].node, "node-ecu-01");
        assert_eq!(plan.endpoints[0].kind, EndpointKind::Tap);
        // A tap has no peer: the hypervisor opens the access port itself.
        assert_eq!(plan.endpoints[0].peer, "vz-node-ecu-01");
        // No `network.data_plane` in BASE — every frozen experiment is in exactly this shape,
        // and this is what keeps them selecting the kernel bridge without ever naming it.
        assert_eq!(plan.data_plane, crate::model::DataPlaneKind::KernelBridge);
        // Determinism: same workspace, same plan.
        assert_eq!(plan, GatewayPlan::from_model(&model(BASE)).unwrap());
    }

    #[test]
    fn data_plane_ovs_reaches_the_plan() {
        let ws = BASE.replacen(
            "network:\n  defaults: { bandwidth: 500000000 }",
            "network:\n  data_plane: ovs\n  defaults: { bandwidth: 500000000 }",
            1,
        );
        let plan = GatewayPlan::from_model(&model(&ws)).unwrap();
        assert_eq!(plan.data_plane, crate::model::DataPlaneKind::Ovs);
        // Selecting OVS changes nothing else about the plan — the same slices/endpoints/HTB
        // rates a campaign would compare across the two mechanisms.
        let kernel_bridge_plan = GatewayPlan::from_model(&model(BASE)).unwrap();
        assert_eq!(plan.slices, kernel_bridge_plan.slices);
        assert_eq!(plan.endpoints, kernel_bridge_plan.endpoints);
    }

    #[test]
    fn rejects_vlan_colliding_with_the_reserved_fiveg_uplink_vlan() {
        let ws = slices_only("    - { id: adas, mode: vlan, vlan: 4000 }");
        let err = GatewayPlan::from_model(&model(&ws)).unwrap_err();
        assert!(matches!(
            err,
            PlanError::VlanCollidesWithFiveGUplink(id, 4000) if id == "adas"
        ));
    }

    #[test]
    fn vlan_only_manifest_has_no_snssai_ids_tracked() {
        // Regression guard: adding s-nssai tracking must not change the
        // vlan-only plan at all (zero risk to the frozen H1/H3/H4 numbers).
        let plan = GatewayPlan::from_model(&model(BASE)).unwrap();
        assert!(plan.snssai_slice_ids.is_empty());
    }

    #[test]
    fn mixed_manifest_tracks_snssai_ids_without_provisioning_them() {
        // 2026-07-17: a workspace mixing vlan and s-nssai slices used to be
        // rejected outright (PlanError::SnssaiNotSupported); it's now
        // accepted, with vlan slices provisioned exactly as before and
        // s-nssai slices merely tracked (fiveg owns their real lifecycle).
        let m = model(&BASE.replacen(
            "    - { id: infotainment, mode: vlan, vlan: 103, qos: { min_rate_mbps: 0, max_rate_mbps: 400, priority: 5 } }",
            "    - { id: infotainment, mode: vlan, vlan: 103, qos: { min_rate_mbps: 0, max_rate_mbps: 400, priority: 5 } }\n    - { id: adas5g, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }",
            1,
        ));
        let plan = GatewayPlan::from_model(&m).expect("mixed workspace now succeeds");
        // vlan slices provisioned exactly as the vlan-only case (BASE).
        assert_eq!(plan.slices.len(), 2);
        assert_eq!(plan.slices[0].id, "adas");
        // s-nssai slice tracked, not turned into a SlicePlan/HTB class.
        assert_eq!(plan.snssai_slice_ids, vec!["adas5g".to_string()]);
    }

    #[test]
    fn snssai_only_manifest_succeeds_with_no_vlan_slices() {
        let ws =
            slices_only("    - { id: adas5g, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }");
        let plan = GatewayPlan::from_model(&model(&ws)).expect("s-nssai-only workspace succeeds");
        assert!(plan.slices.is_empty());
        assert_eq!(plan.snssai_slice_ids, vec!["adas5g".to_string()]);
        // Bridge/uplink are still planned — an s-nssai-only experiment still
        // needs the bridge for fiveg's transit veth to attach to.
        assert_eq!(plan.bridge, "vxbr0");
    }

    /// The H3 shape, and the regression the derivation exists to prevent: exp-002's three arms
    /// used to be `vz0`/`vz1`/`vz2` in traversal order, and the two schema paths traversed
    /// differently — so two arms swapped interfaces and the campaign reported microvm numbers
    /// measured on libvirt. Spelled out because these are the names campaign scripts hardcode,
    /// and because the peer differs by backend (a container gets a veth peer, a VM's peer *is*
    /// its access port), so a swap would not even fail an interface-exists check.
    #[test]
    fn each_arm_gets_the_interface_its_device_id_derives() {
        let ws = r#"
kind: experiment
id: exp-h3
vehicles: [veh]
network: { slices: [{ id: s, mode: vlan, vlan: 101 }] }
---
kind: vehicle
id: veh
devices:
  node-cont-01:    { role: ecu, backend: container }
  node-microvm-01: { role: ecu, backend: qemu-microvm }
  node-libvirt-01: { role: ecu, backend: libvirt }
links:
  - { id: a, endpoints: [node-cont-01:eth0],    slice: s }
  - { id: b, endpoints: [node-microvm-01:eth0], slice: s }
  - { id: c, endpoints: [node-libvirt-01:eth0], slice: s }
"#;
        let plan = GatewayPlan::from_model(&model(ws)).unwrap();
        let named: Vec<(&str, &str, &str)> = plan
            .endpoints
            .iter()
            .map(|e| (e.node.as_str(), e.name.as_str(), e.peer.as_str()))
            .collect();
        assert_eq!(
            named,
            vec![
                ("node-cont-01", "vz-node-c-32cc", "vzp-node-c-32cc"),
                ("node-libvirt-01", "vz-node-l-de44", "vz-node-l-de44"),
                ("node-microvm-01", "vz-node-m-a34f", "vz-node-m-a34f"),
            ]
        );
    }

    /// Two device ids that shorten to one interface name are refused rather than provisioned
    /// over each other — the failure mode shortening introduces, caught where it can be named.
    #[test]
    fn two_devices_deriving_one_interface_name_are_refused() {
        let ws = r#"
kind: experiment
id: e
vehicles: [veh]
network: { slices: [{ id: s, mode: vlan, vlan: 101 }] }
---
kind: vehicle
id: veh
devices:
  node.a: { role: ecu, backend: container }
  node-a: { role: ecu, backend: container }
links:
  - { id: a, endpoints: [node.a:eth0], slice: s }
  - { id: b, endpoints: [node-a:eth0], slice: s }
"#;
        let err = GatewayPlan::from_model(&model(ws)).unwrap_err();
        assert!(
            matches!(&err, PlanError::EndpointNameCollision { name, .. } if name == "vz-node-a"),
            "{err}"
        );
    }

    /// A vehicle whose two links carry different harness characteristics — a 100BASE-T1 sensor
    /// run and a 1000BASE-T1 backbone segment, which is the comparison §7 says the model exists
    /// to make iterable.
    const IMPAIRED: &str = r#"
kind: experiment
id: exp-imp
vehicles: [veh]
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
    - { id: infotainment, mode: vlan, vlan: 103 }
---
kind: vehicle
id: veh
devices:
  node-sensor-01: { role: sensor, backend: container }
  node-hpc-01:    { role: hpc, backend: container }
links:
  - id: seg-t1
    endpoints: [node-sensor-01:eth0]
    slice: adas
    impairments: { rate_mbps: 100, delay_us: 250, jitter_us: 40, loss_pct: 0.01 }
  - id: seg-backbone
    endpoints: [node-hpc-01:eth0]
    slice: infotainment
    impairments: { rate_mbps: 1000 }
"#;

    #[test]
    fn an_impaired_link_lands_on_its_access_port_with_its_ifb() {
        let plan = GatewayPlan::from_model(&model(IMPAIRED)).unwrap();
        let sensor = plan
            .endpoints
            .iter()
            .find(|e| e.node == "node-sensor-01")
            .unwrap();
        let imp = sensor.impairment.as_ref().expect("seg-t1 is impaired");
        assert_eq!(imp.link, "seg-t1");
        assert_eq!(imp.ifb, "vxi-node-s-c9ce");
        assert_eq!(sensor.name, "vz-node-s-c9ce");
        assert_eq!(imp.params.rate_mbps, Some(100));
        assert_eq!(imp.params.delay_us, Some(250));
        assert_eq!(imp.params.jitter_us, Some(40));
        // 0.01% is 100 ppm — the conversion the model does once, checked where it is used.
        assert_eq!(imp.params.loss_ppm, Some(100));

        let hpc = plan
            .endpoints
            .iter()
            .find(|e| e.node == "node-hpc-01")
            .unwrap();
        let imp = hpc.impairment.as_ref().expect("seg-backbone is impaired");
        assert_eq!(imp.params.rate_mbps, Some(1000));
        assert_eq!(imp.params.delay_us, None);

        assert_eq!(plan, GatewayPlan::from_model(&model(IMPAIRED)).unwrap());
    }

    /// The regression the one-port rule exists to prevent, and the reason it is not "impair every
    /// attachment": a link between two devices on this bridge would be crossed through *two*
    /// impaired ports, so a 250 µs wire would measure 500 µs with nothing reporting a problem.
    #[test]
    fn a_link_between_two_devices_impairs_one_port_not_both() {
        let ws = r#"
kind: experiment
id: e
vehicles: [veh]
network: { slices: [{ id: adas, mode: vlan, vlan: 101 }] }
---
kind: vehicle
id: veh
devices:
  gw1:    { role: gateway, backend: container }
  zonefl: { role: zone-controller, backend: container }
links:
  - id: trunk-fl
    endpoints: [gw1:eth0, zonefl:eth0]
    slice: adas
    impairments: { delay_us: 250 }
"#;
        let plan = GatewayPlan::from_model(&model(ws)).unwrap();
        assert_eq!(plan.endpoints.len(), 2, "both attachments still get a port");
        let impaired: Vec<&str> = plan
            .endpoints
            .iter()
            .filter(|e| e.impairment.is_some())
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(
            impaired,
            vec!["vz-gw1"],
            "exactly one port carries the netem"
        );
    }

    /// Election is by lowest interface name, so it cannot be moved by reordering the endpoint
    /// list — the same property `ifnames` gives every other name here.
    #[test]
    fn the_carrier_port_does_not_depend_on_endpoint_order() {
        let one = r#"
kind: experiment
id: e
vehicles: [veh]
network: { slices: [{ id: adas, mode: vlan, vlan: 101 }] }
---
kind: vehicle
id: veh
devices:
  aaa: { role: ecu, backend: container }
  zzz: { role: ecu, backend: container }
links:
  - { id: l, endpoints: [ENDPOINTS], slice: adas, impairments: { delay_us: 250 } }
"#;
        let carrier = |eps: &str| {
            let plan = GatewayPlan::from_model(&model(&one.replace("ENDPOINTS", eps))).unwrap();
            plan.endpoints
                .iter()
                .find(|e| e.impairment.is_some())
                .map(|e| e.name.clone())
                .unwrap()
        };
        assert_eq!(carrier("aaa:eth0, zzz:eth0"), "vz-aaa");
        assert_eq!(carrier("zzz:eth0, aaa:eth0"), "vz-aaa");
    }

    /// Two links impairing one port would leave only one netem installed and the run would
    /// quietly carry characteristics nobody declared. It cannot arise — one device attached to
    /// two links on the same port is already refused where two attachments derive one interface
    /// name, which is why `assign_impairments` carries no collision check of its own. Pinned
    /// here so that reasoning has a test holding it up rather than a comment asserting it.
    #[test]
    fn one_port_impaired_by_two_links_is_already_refused_upstream() {
        let ws = r#"
kind: experiment
id: e
vehicles: [veh]
network: { slices: [{ id: adas, mode: vlan, vlan: 101 }] }
---
kind: vehicle
id: veh
devices:
  node-a: { role: ecu, backend: container }
links:
  - { id: l1, endpoints: [node-a:eth0], slice: adas, impairments: { delay_us: 250 } }
  - { id: l2, endpoints: [node-a:eth0], slice: adas, impairments: { delay_us: 900 } }
"#;
        let err = GatewayPlan::from_model(&model(ws)).unwrap_err();
        assert!(
            matches!(&err, PlanError::EndpointNameCollision { name, .. } if name == "vz-node-a"),
            "{err}"
        );
    }

    /// Impairments on an s-nssai link are refused rather than dropped.
    ///
    /// This is the case the planner's backstop is really for, and it is a plausible thing to
    /// write: the link looks provisionable, binds a defined slice and attaches a real device. But
    /// an s-nssai slice carries no VLAN, so this reconciler plans no access port for it — fiveg
    /// owns that lifecycle — and there is nowhere to put the netem. Silently, the workspace would
    /// have declared a 100BASE-T1 segment and measured a clean one.
    #[test]
    fn an_impairment_on_an_snssai_link_is_refused_rather_than_dropped() {
        let ws = r#"
kind: experiment
id: e
vehicles: [veh]
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
    - { id: adas5g, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }
---
kind: vehicle
id: veh
devices:
  node-a: { role: ecu, backend: container }
  node-b: { role: ecu, backend: container }
links:
  - { id: l1, endpoints: [node-a:eth0], slice: adas }
  - { id: l2, endpoints: [node-b:eth0], slice: adas5g, impairments: { delay_us: 250 } }
"#;
        let err = GatewayPlan::from_model(&model(ws)).unwrap_err();
        assert!(
            matches!(&err, PlanError::ImpairmentNotRenderable { link } if link == "l2"),
            "{err}"
        );
    }

    /// Regression guard, matching the s-nssai one above: the frozen experiments declare no
    /// impairments, and adding the feature must leave their plans byte for byte unchanged.
    #[test]
    fn an_unimpaired_workspace_plans_no_impairments() {
        let plan = GatewayPlan::from_model(&model(BASE)).unwrap();
        assert!(plan.endpoints.iter().all(|e| e.impairment.is_none()));
        assert!(!plan.summary().contains("impaired"));
    }

    #[test]
    fn rejects_manifest_without_any_slices() {
        let ws =
            "kind: experiment\nid: e\nvehicles: [veh]\n---\nkind: vehicle\nid: veh\ndevices: {}\n";
        assert!(matches!(
            GatewayPlan::from_model(&model(ws)),
            Err(PlanError::NoSlices)
        ));
    }
}
