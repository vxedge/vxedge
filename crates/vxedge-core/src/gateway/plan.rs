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
                    });
                }
            }
        }

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
        format!(
            "bridge {} + uplink {} @ {} mbit; slices: [{slices}]{snssai}; {} endpoint(s)",
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
        // Determinism: same workspace, same plan.
        assert_eq!(plan, GatewayPlan::from_model(&model(BASE)).unwrap());
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
