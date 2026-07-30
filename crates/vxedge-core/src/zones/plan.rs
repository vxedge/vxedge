//! NodePlan: desired state for one node interface, joined 1:1 with the
//! gateway plan's endpoints (same deterministic traversal order — that
//! invariant is asserted, not assumed).

use std::path::{Path, PathBuf};

use crate::gateway::plan::{EndpointKind, GatewayPlan};
use crate::manifest::Backend;

#[derive(Debug, thiserror::Error)]
pub enum ZonePlanError {
    #[error(
        "gateway/zone plan mismatch: {gateway} gateway endpoints vs {zones} zone node ifaces — \
         plans must come from the same manifest"
    )]
    EndpointMismatch { gateway: usize, zones: usize },
    #[error("node `{0}`: backend `{1:?}` requires an image bundle path (kind vm/rtimage)")]
    MissingBundle(String, Backend),
    #[error(
        "device `{0}` declares no `backend`, so there is no way to know whether it attaches by \
         veth or by tap. Add `backend: container|qemu-microvm|libvirt`."
    )]
    DeviceWithoutBackend(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePlan {
    pub zone_id: String,
    pub node_id: String,
    pub backend: Backend,
    /// Bridge access port (tap for VMs; veth host side for containers).
    pub endpoint: String,
    /// Veth peer that moves into the container netns (== endpoint for taps).
    pub peer: String,
    pub kind: EndpointKind,
    pub vlan: u16,
    pub slice: Option<String>,
    /// Node address (CIDR) from the manifest, if declared.
    pub ip_cidr: Option<String>,
    /// For vm/rtimage kinds: guest bundle dir (vmlinuz + initrd.img),
    /// resolved against the manifest's directory when relative.
    pub bundle: Option<PathBuf>,
    pub pinned_cpus: Vec<u32>,
}

impl NodePlan {
    /// Stable, IFNAMSIZ-safe-ish runtime name (netns / domain / pidfile).
    pub fn runtime_name(&self) -> String {
        let mut s: String = self
            .node_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        s.truncate(48);
        format!("vx-{s}")
    }
}

/// Join model devices with the gateway plan's endpoints (task 5b).
///
/// Traversal must match [`GatewayPlan::from_model`]'s exactly — sorted device id, then the links
/// touching it — because the two plans are joined 1:1 by index. That invariant is asserted here
/// rather than assumed, the same way the §6.1 path does it.
///
/// `zone_id` becomes the **region** the device sits in, not a per-device wrapper. That is the
/// model's whole point, and it gives `vxedge zone up --vxid front-left` a real meaning: bring up
/// everything in that region. A device belonging to no region (a gateway, a central computer)
/// falls back to its own id so the field is never empty.
pub fn plan_nodes_from_model(
    exp: &crate::model::Experiment,
    gateway: &GatewayPlan,
    workspace_dir: &Path,
) -> Result<Vec<NodePlan>, ZonePlanError> {
    let mut plans = Vec::new();
    let mut idx = 0usize;
    for entity in &exp.entities {
        for device in entity.devices_in_order() {
            for (link, endpoint) in entity.links_for(&device.id) {
                // Join only what `GatewayPlan::from_model` produced an endpoint for: a slice
                // with a vlan. An `s-nssai` attachment is slice-bound but has no vlan and no
                // bridge port — its lifecycle belongs to `fiveg`, not here — so counting it
                // would desynchronise the 1:1 index join with the gateway plan.
                let is_vlan_attached = link
                    .slice
                    .as_deref()
                    .and_then(|s| exp.slices.iter().find(|sl| sl.id == s))
                    .and_then(|sl| sl.vlan)
                    .is_some();
                if !is_vlan_attached {
                    continue;
                }
                let Some(ep) = gateway.endpoints.get(idx) else {
                    return Err(ZonePlanError::EndpointMismatch {
                        gateway: gateway.endpoints.len(),
                        zones: idx + 1,
                    });
                };
                debug_assert_eq!(ep.node, device.id, "traversal order invariant");

                // A VM-ish backend boots a bundle directory; a container has no image to
                // resolve. The backend is the authority here — deriving it from the part
                // reference is the guess that produced an unbootable manifest during the
                // exp-001 migration.
                let backend = match device.backend {
                    Some(crate::model::Backend::Container) => Backend::Container,
                    Some(crate::model::Backend::Libvirt) => Backend::Libvirt,
                    Some(crate::model::Backend::QemuMicrovm) => Backend::QemuMicrovm,
                    // GatewayPlan::from_model refuses this too, so reaching it means the two
                    // plans came from different models. Reported as what it is: an
                    // "endpoint mismatch" here would point at plan joining and send someone
                    // looking in entirely the wrong place.
                    None => {
                        return Err(ZonePlanError::DeviceWithoutBackend(device.id.clone()));
                    }
                };
                // A VM backend boots a bundle. A part that is a container image is not one, and
                // saying so is the check §6.1 made through `image.kind` — losing it let a
                // libvirt node pointed at a container image plan cleanly and fail at boot.
                let bundle = match backend {
                    Backend::Libvirt | Backend::QemuMicrovm => entity
                        .functions
                        .get(&device.id)
                        .and_then(|f| f.part.as_ref())
                        .filter(|part| part.kind == crate::model::PartKind::Bundle)
                        .map(|part| {
                            let p = Path::new(&part.reference);
                            if p.is_absolute() {
                                p.to_path_buf()
                            } else {
                                workspace_dir.join(p)
                            }
                        }),
                    Backend::Container => None,
                };
                if bundle.is_none() && matches!(backend, Backend::Libvirt | Backend::QemuMicrovm) {
                    return Err(ZonePlanError::MissingBundle(device.id.clone(), backend));
                }

                plans.push(NodePlan {
                    zone_id: device.zone.clone().unwrap_or_else(|| device.id.clone()),
                    node_id: device.id.clone(),
                    backend,
                    endpoint: ep.name.clone(),
                    peer: ep.peer.clone(),
                    kind: ep.kind,
                    vlan: ep.vlan,
                    slice: ep.slice.clone(),
                    ip_cidr: endpoint.ip_cidr.clone(),
                    bundle,
                    pinned_cpus: device.pinned_cpus.clone(),
                });
                idx += 1;
            }
        }
    }
    if idx != gateway.endpoints.len() {
        return Err(ZonePlanError::EndpointMismatch {
            gateway: gateway.endpoints.len(),
            zones: idx,
        });
    }
    Ok(plans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ExperimentManifest;

    const YAML: &str = r#"
experiment: { id: exp-z, name: z }
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }
zones:
  - id: vxzone-fl-01
    role: front-left
    nodes:
      - id: node-a
        type: ecu
        image: { url: images/linux-probe/bundle, kind: rtimage }
        backend: qemu-microvm
        network: [{ iface: eth0, vlan: 101, slice: adas, ip: 10.101.0.2/24 }]
      - id: node-b
        type: ecu
        image: { url: whatever, kind: container }
        backend: container
        network: [{ iface: eth0, vlan: 101, slice: adas, ip: 10.101.0.3/24 }]
"#;

    #[test]
    fn joins_nodes_with_gateway_endpoints() {
        let m = ExperimentManifest::from_yaml(YAML).unwrap();
        let gw = GatewayPlan::from_model(&m.to_model().expect("adapts")).unwrap();
        let plans =
            plan_nodes_from_model(&m.to_model().expect("adapts"), &gw, Path::new("/manifests"))
                .unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].backend, Backend::QemuMicrovm);
        assert_eq!(plans[0].kind, EndpointKind::Tap);
        assert_eq!(plans[0].endpoint, "vz0");
        assert_eq!(
            plans[0].bundle.as_deref(),
            Some(Path::new("/manifests/images/linux-probe/bundle"))
        );
        assert_eq!(plans[0].ip_cidr.as_deref(), Some("10.101.0.2/24"));
        assert_eq!(plans[1].backend, Backend::Container);
        assert_eq!(plans[1].kind, EndpointKind::Veth);
        assert_eq!(plans[1].peer, "vzp1");
        assert_eq!(plans[1].runtime_name(), "vx-node-b");
    }

    #[test]
    fn vm_backend_without_bundle_is_rejected() {
        let bad = YAML.replace(
            "image: { url: whatever, kind: container }\n        backend: container",
            "image: { url: whatever, kind: container }\n        backend: qemu-microvm",
        );
        let m = ExperimentManifest::from_yaml(&bad).unwrap();
        let gw = GatewayPlan::from_model(&m.to_model().expect("adapts")).unwrap();
        assert!(matches!(
            plan_nodes_from_model(&m.to_model().expect("adapts"), &gw, Path::new("/x")),
            Err(ZonePlanError::MissingBundle(id, _)) if id == "node-b"
        ));
    }
}
