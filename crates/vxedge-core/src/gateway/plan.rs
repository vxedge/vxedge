//! GatewayPlan: the desired-state compilation of a manifest's network section.
//! Pure data — no privileges needed — so `Apply` (plan/diff) and unit tests
//! run anywhere. Interface names are deterministic per manifest ordering,
//! which is what lets the reconciler match observed state on re-runs.

use crate::manifest::{Backend, ExperimentManifest, ImageKind, Node, SliceMode};

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

/// Resolve the effective zone backend for a node (§9: libvirt is the default
/// for vm/rtimage image kinds; containers default to the container backend).
pub fn resolved_backend(node: &Node) -> Backend {
    node.backend.unwrap_or(match node.image.kind {
        ImageKind::Container => Backend::Container,
        ImageKind::Vm | ImageKind::Rtimage => Backend::Libvirt,
    })
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

/// A slice as the planner needs it, independent of which schema it came from. Exists so the
/// §6.1 and model paths feed the *same* planner and cannot drift in class minors or defaults.
struct SliceInput {
    id: String,
    is_vlan: bool,
    vlan: Option<u16>,
    min_rate_mbps: u64,
    max_rate_mbps: u64,
    priority: u8,
}

/// Slice planning, shared by both constructors so the HTB class ids, minor numbers and default
/// rates cannot drift between the §6.1 and model paths while both exist.
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
    /// Plan from the domain model (task 5b). The §6.1 path below stays until every experiment
    /// has migrated; both produce the same `GatewayPlan`, which is what makes the migration
    /// checkable rather than merely plausible.
    ///
    /// **Endpoint numbering is positional** — `vz0`, `vz1`, … in traversal order — and campaign
    /// scripts address those names directly. Traversal here is sorted device id, because §6.0
    /// stores components in a map and declaration order is not preserved. That is deterministic
    /// and reproducible, but it does mean adding a device whose id sorts earlier renumbers the
    /// rest. The §6.1 path has the same hazard (adding a zone renumbers), so this is not a
    /// regression — it is a fragility worth removing later, tracked in TASKS.md.
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

        let mut endpoints = Vec::new();
        for entity in &exp.entities {
            for device in entity.devices_in_order() {
                for (link, _) in entity.links_for(&device.id) {
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
                    let idx = endpoints.len();
                    let name = format!("vz{idx}");
                    endpoints.push(EndpointPlan {
                        peer: match kind {
                            EndpointKind::Veth => format!("vzp{idx}"),
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

    pub fn from_manifest(manifest: &ExperimentManifest) -> Result<Self, PlanError> {
        let network = manifest.network.as_ref();
        let manifest_slices = network.map(|n| n.slices.as_slice()).unwrap_or(&[]);

        // Routed through the same planner as the model path, so class minors and default rates
        // cannot drift between the two while both exist.
        let flat: Vec<SliceInput> = manifest_slices
            .iter()
            .map(|s| {
                let q = s.qos.as_ref();
                SliceInput {
                    id: s.id.clone(),
                    is_vlan: s.mode == SliceMode::Vlan,
                    vlan: s.vlan,
                    min_rate_mbps: q.and_then(|q| q.min_rate_mbps).unwrap_or(0),
                    max_rate_mbps: q.and_then(|q| q.max_rate_mbps).unwrap_or(0),
                    priority: q.and_then(|q| q.priority).unwrap_or(4),
                }
            })
            .collect();
        let (slices, snssai_slice_ids) = plan_slices(&flat)?;

        let mut endpoints = Vec::new();
        for zone in &manifest.zones {
            for node in &zone.nodes {
                let kind = match resolved_backend(node) {
                    Backend::Container => EndpointKind::Veth,
                    Backend::Libvirt | Backend::QemuMicrovm => EndpointKind::Tap,
                };
                for iface in &node.network {
                    if let Some(vlan) = iface.vlan {
                        let idx = endpoints.len();
                        let name = format!("vz{idx}");
                        endpoints.push(EndpointPlan {
                            peer: match kind {
                                EndpointKind::Veth => format!("vzp{idx}"),
                                EndpointKind::Tap => name.clone(),
                            },
                            name,
                            kind,
                            node: node.id.clone(),
                            vlan,
                            slice: iface.slice.clone(),
                        });
                    }
                }
            }
        }

        let root_rate_mbps = network
            .and_then(|n| n.defaults.as_ref())
            .and_then(|d| d.bandwidth)
            .map(|bps| (bps / 1_000_000).max(1))
            .unwrap_or(1000);

        Ok(Self {
            bridge: "vxbr0".into(),
            uplink: "vxup0".into(),
            uplink_peer: "vxup0p".into(),
            root_rate_mbps,
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

    fn manifest(yaml: &str) -> ExperimentManifest {
        ExperimentManifest::from_yaml(yaml).expect("valid test manifest")
    }

    const BASE: &str = r#"
experiment: { id: exp-t, name: t }
network:
  defaults: { bandwidth: 500000000 }
  slices:
    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }
    - { id: infotainment, mode: vlan, vlan: 103, qos: { min_rate_mbps: 0, max_rate_mbps: 400, priority: 5 } }
zones:
  - id: vxzone-fl-01
    role: front-left
    nodes:
      - id: node-ecu-01
        type: ecu
        image: { url: x, kind: rtimage }
        network: [{ iface: eth0, vlan: 101, slice: adas }]
      - id: node-info-01
        type: infotainment
        image: { url: x, kind: vm }
        network: [{ iface: eth0, vlan: 103, slice: infotainment }]
"#;

    #[test]
    fn builds_deterministic_plan() {
        let plan = GatewayPlan::from_manifest(&manifest(BASE)).unwrap();
        assert_eq!(plan.bridge, "vxbr0");
        assert_eq!(plan.root_rate_mbps, 500);
        assert_eq!(plan.slices.len(), 2);
        assert_eq!(plan.slices[0].vlan, 101);
        assert_eq!(plan.slices[0].class_minor, 0x10);
        assert_eq!(plan.slices[1].class_minor, 0x11);
        // min rate 0 floors to 1 mbit (HTB needs non-zero rates).
        assert_eq!(plan.slices[1].min_rate_mbps, 1);
        assert_eq!(plan.endpoints.len(), 2);
        assert_eq!(plan.endpoints[0].name, "vz0");
        assert_eq!(plan.endpoints[0].node, "node-ecu-01");
        // No explicit backend: rtimage/vm image kinds default to libvirt => tap.
        assert_eq!(plan.endpoints[0].kind, EndpointKind::Tap);
        assert_eq!(plan.endpoints[0].peer, "vz0");
        // Determinism: same manifest, same plan.
        assert_eq!(plan, GatewayPlan::from_manifest(&manifest(BASE)).unwrap());
    }

    #[test]
    fn rejects_vlan_colliding_with_the_reserved_fiveg_uplink_vlan() {
        let yaml = r#"
experiment: { id: exp-t, name: t }
network:
  slices:
    - { id: adas, mode: vlan, vlan: 4000 }
"#;
        let err = GatewayPlan::from_manifest(&manifest(yaml)).unwrap_err();
        assert!(matches!(
            err,
            PlanError::VlanCollidesWithFiveGUplink(id, 4000) if id == "adas"
        ));
    }

    #[test]
    fn vlan_only_manifest_has_no_snssai_ids_tracked() {
        // Regression guard: adding s-nssai tracking must not change the
        // vlan-only plan at all (zero risk to the frozen H1/H3/H4 numbers).
        let plan = GatewayPlan::from_manifest(&manifest(BASE)).unwrap();
        assert!(plan.snssai_slice_ids.is_empty());
    }

    #[test]
    fn mixed_manifest_tracks_snssai_ids_without_provisioning_them() {
        // 2026-07-17: a manifest mixing vlan and s-nssai slices used to be
        // rejected outright (PlanError::SnssaiNotSupported); it's now
        // accepted, with vlan slices provisioned exactly as before and
        // s-nssai slices merely tracked (fiveg owns their real lifecycle).
        let m = manifest(&BASE.replacen(
            "    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }",
            "    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }\n    - { id: adas5g, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }",
            1,
        ));
        let plan = GatewayPlan::from_manifest(&m).expect("mixed manifest now succeeds");
        // vlan slices provisioned exactly as the vlan-only case (BASE).
        assert_eq!(plan.slices.len(), 2);
        assert_eq!(plan.slices[0].id, "adas");
        // s-nssai slice tracked, not turned into a SlicePlan/HTB class.
        assert_eq!(plan.snssai_slice_ids, vec!["adas5g".to_string()]);
    }

    #[test]
    fn snssai_only_manifest_succeeds_with_no_vlan_slices() {
        let m = manifest(
            "experiment: { id: e, name: n }\nnetwork:\n  slices:\n    - { id: adas5g, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }\n",
        );
        let plan = GatewayPlan::from_manifest(&m).expect("s-nssai-only manifest now succeeds");
        assert!(plan.slices.is_empty());
        assert_eq!(plan.snssai_slice_ids, vec!["adas5g".to_string()]);
        // Bridge/uplink are still planned — an s-nssai-only experiment still
        // needs the bridge for fiveg's transit veth to attach to.
        assert_eq!(plan.bridge, "vxbr0");
    }

    #[test]
    fn rejects_manifest_without_any_slices() {
        let m = manifest("experiment: { id: e, name: n }\n");
        assert!(matches!(
            GatewayPlan::from_manifest(&m),
            Err(PlanError::NoSlices)
        ));
    }
}
