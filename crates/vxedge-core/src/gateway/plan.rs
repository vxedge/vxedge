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
        "no `network.slices` with mode `vlan` — nothing to provision (S-NSSAI slices are phase 2)"
    )]
    NoVlanSlices,
    #[error("slice `{0}` uses mode `s-nssai`, which is not provisionable yet (phase 2)")]
    SnssaiNotSupported(String),
    #[error("too many slices: {0} (max {MAX_SLICES})")]
    TooManySlices(usize),
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
}

impl GatewayPlan {
    pub fn from_manifest(manifest: &ExperimentManifest) -> Result<Self, PlanError> {
        let network = manifest.network.as_ref();
        let manifest_slices = network.map(|n| n.slices.as_slice()).unwrap_or(&[]);

        if let Some(s) = manifest_slices.iter().find(|s| s.mode == SliceMode::Snssai) {
            return Err(PlanError::SnssaiNotSupported(s.id.clone()));
        }
        if manifest_slices.len() > MAX_SLICES {
            return Err(PlanError::TooManySlices(manifest_slices.len()));
        }

        let slices: Vec<SlicePlan> = manifest_slices
            .iter()
            .filter(|s| s.mode == SliceMode::Vlan)
            .enumerate()
            .map(|(i, s)| {
                let qos = s.qos.as_ref();
                SlicePlan {
                    id: s.id.clone(),
                    vlan: s.vlan.expect("validated: vlan slice has vlan id"),
                    min_rate_mbps: qos.and_then(|q| q.min_rate_mbps).unwrap_or(0).max(1),
                    max_rate_mbps: qos.and_then(|q| q.max_rate_mbps).unwrap_or(0).max(1),
                    priority: qos.and_then(|q| q.priority).unwrap_or(4),
                    class_minor: 0x10 + i as u16,
                }
            })
            .collect();
        if slices.is_empty() {
            return Err(PlanError::NoVlanSlices);
        }

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
        format!(
            "bridge {} + uplink {} @ {} mbit; slices: [{slices}]; {} endpoint(s)",
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
    fn rejects_snssai_slices_for_now() {
        let yaml = BASE.replace(
            "{ id: adas, mode: vlan, vlan: 101,",
            "{ id: adas, mode: s-nssai, s_nssai: { sst: 1 },",
        );
        // Manifest itself is valid; the *plan* rejects it as phase 2.
        let m = manifest(&yaml);
        assert!(matches!(
            GatewayPlan::from_manifest(&m),
            Err(PlanError::SnssaiNotSupported(id)) if id == "adas"
        ));
    }

    #[test]
    fn rejects_manifest_without_slices() {
        let m = manifest("experiment: { id: e, name: n }\n");
        assert!(matches!(
            GatewayPlan::from_manifest(&m),
            Err(PlanError::NoVlanSlices)
        ));
    }
}
