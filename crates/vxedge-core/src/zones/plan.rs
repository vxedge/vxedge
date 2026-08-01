//! NodePlan: desired state for one node interface, joined 1:1 with the
//! gateway plan's endpoints **by interface name** — which is derived from the
//! device id (`gateway::ifnames`), so the join is on identity rather than on
//! two traversals happening to agree.

use std::path::{Path, PathBuf};

use crate::gateway::plan::{EndpointKind, GatewayPlan};
use crate::model::{Backend, DeviceRole};

#[derive(Debug, thiserror::Error)]
pub enum ZonePlanError {
    #[error(
        "gateway/zone plan mismatch: {gateway} gateway endpoints vs {zones} zone node ifaces — \
         plans must come from the same manifest"
    )]
    EndpointMismatch { gateway: usize, zones: usize },
    #[error(
        "device `{device}` attaches to slice `{slice}` as `{iface}`, which the gateway plan does \
         not contain — the two plans came from different models"
    )]
    NoSuchEndpoint {
        device: String,
        slice: String,
        iface: String,
    },
    #[error("node `{0}`: backend `{1:?}` requires an image bundle path (kind vm/rtimage)")]
    MissingBundle(String, Backend),
    #[error(
        "device `{device}` hosts {count} functions carrying a bootable bundle, and a VM boots \
         one. Deploy the others on their own devices, or publish them as a single `stack`-layer \
         part."
    )]
    AmbiguousBundle { device: String, count: usize },
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
    /// The device's declared role. `ZoneService` and `ComputeService` share this one plan and
    /// this one lifecycle (`zones::node_up`/`node_down`/`node_status`) — the two RPC surfaces
    /// differ only in which role they address, matching the OEM vocabulary split between zone
    /// controllers/ECUs and central compute, not a difference in how either is actually
    /// provisioned.
    pub role: DeviceRole,
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

/// Join model devices with the gateway plan's endpoints.
///
/// The join is **by interface name**, which both sides derive from the device id
/// ([`crate::gateway::ifnames`]). It used to be by index, on the assumption that two traversals
/// of the same model visit attachments in the same order — true, but an assumption that had to
/// be re-established by hand every time either loop changed, and one whose failure mode was a
/// device provisioned onto another device's interface.
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
    for entity in &exp.entities {
        for device in entity.devices.values() {
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
                let iface =
                    crate::gateway::ifnames::access_port(&device.id, endpoint.port.as_deref());
                let Some(ep) = gateway.endpoints.iter().find(|e| e.name == iface) else {
                    return Err(ZonePlanError::NoSuchEndpoint {
                        device: device.id.clone(),
                        slice: link.slice.clone().unwrap_or_default(),
                        iface,
                    });
                };

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
                    Backend::Libvirt | Backend::QemuMicrovm => {
                        let bundles: Vec<&crate::model::Part> = entity
                            .functions_on(&device.id)
                            .filter_map(|f| f.part.as_ref())
                            .filter(|part| part.kind == crate::model::PartKind::Bundle)
                            .collect();
                        if bundles.len() > 1 {
                            return Err(ZonePlanError::AmbiguousBundle {
                                device: device.id.clone(),
                                count: bundles.len(),
                            });
                        }
                        bundles.first().map(|part| {
                            let p = Path::new(&part.reference);
                            if p.is_absolute() {
                                p.to_path_buf()
                            } else {
                                workspace_dir.join(p)
                            }
                        })
                    }
                    Backend::Container => None,
                };
                if bundle.is_none() && matches!(backend, Backend::Libvirt | Backend::QemuMicrovm) {
                    return Err(ZonePlanError::MissingBundle(device.id.clone(), backend));
                }

                plans.push(NodePlan {
                    zone_id: device.zone.clone().unwrap_or_else(|| device.id.clone()),
                    node_id: device.id.clone(),
                    role: device.role,
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
            }
        }
    }
    // Every gateway endpoint must have found a node: an endpoint with nothing to attach would
    // leave a bridge port dangling, shaped and counted but connected to nothing.
    if plans.len() != gateway.endpoints.len() {
        return Err(ZonePlanError::EndpointMismatch {
            gateway: gateway.endpoints.len(),
            zones: plans.len(),
        });
    }
    Ok(plans)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = r#"
kind: experiment
id: exp-z
vehicles: [veh]
baseline: bl
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }
---
kind: vehicle
id: veh
zones: { front-left: {} }
devices:
  node-a: { role: ecu, zone: front-left, backend: qemu-microvm }
  node-b: { role: ecu, zone: front-left, backend: container }
functions:
  fn-a: { deployed_on: node-a }
  fn-b: { deployed_on: node-b }
links:
  - { id: la, endpoints: [{ node: node-a, port: eth0, ip: 10.101.0.2/24 }], slice: adas }
  - { id: lb, endpoints: [{ node: node-b, port: eth0, ip: 10.101.0.3/24 }], slice: adas }
---
kind: baseline
id: bl
parts:
  - { ref: ./images/linux-probe/bundle, kind: bundle, target_ids: fn-a }
  - { ref: 'local/vxprobe:host-process', target_ids: fn-b }
"#;

    fn plans(ws: &str) -> Vec<NodePlan> {
        let model = crate::model::load_yaml(ws).expect("valid test workspace");
        let gw = GatewayPlan::from_model(&model).unwrap();
        plan_nodes_from_model(&model, &gw, Path::new("/manifests")).unwrap()
    }

    #[test]
    fn joins_nodes_with_gateway_endpoints() {
        let plans = plans(WS);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].node_id, "node-a");
        assert_eq!(plans[0].backend, Backend::QemuMicrovm);
        assert_eq!(plans[0].kind, EndpointKind::Tap);
        assert_eq!(plans[0].endpoint, "vz-node-a");
        assert_eq!(
            plans[0].bundle.as_deref(),
            Some(Path::new("/manifests/./images/linux-probe/bundle"))
        );
        assert_eq!(plans[0].ip_cidr.as_deref(), Some("10.101.0.2/24"));
        assert_eq!(plans[1].backend, Backend::Container);
        assert_eq!(plans[1].kind, EndpointKind::Veth);
        assert_eq!(plans[1].peer, "vzp-node-b");
        assert_eq!(plans[1].runtime_name(), "vx-node-b");
        // Both devices in this fixture are `role: ecu` — the role a plan reports is not
        // guessed or defaulted, it comes straight from the device declaration.
        assert_eq!(plans[0].role, crate::model::DeviceRole::Ecu);
        assert_eq!(plans[1].role, crate::model::DeviceRole::Ecu);
    }

    /// `ComputeService` and `ZoneService` share this one plan; role is what tells a compute
    /// node apart from a zone-controller/ECU one when a caller needs to filter by it (both
    /// device kinds join the same gateway plan identically, so nothing else here would).
    #[test]
    fn a_mix_of_roles_is_reported_faithfully() {
        let ws = WS.replace(
            "  node-a: { role: ecu, zone: front-left, backend: qemu-microvm }",
            "  node-a: { role: hpc, zone: front-left, backend: qemu-microvm }",
        );
        let plans = plans(&ws);
        assert_eq!(plans[0].role, crate::model::DeviceRole::Hpc);
        assert_eq!(plans[1].role, crate::model::DeviceRole::Ecu);
    }

    /// A VM with nothing deployed on it has no bundle to boot. Reported as such rather than
    /// booted empty — `MissingBundle` is what a mis-wired H3 arm looks like from here.
    #[test]
    fn a_vm_backend_with_nothing_to_boot_is_rejected() {
        let ws = WS
            .replace("  fn-b: { deployed_on: node-b }\n", "")
            .replace(
                "  - { ref: 'local/vxprobe:host-process', target_ids: fn-b }\n",
                "",
            )
            .replace(
                "  node-b: { role: ecu, zone: front-left, backend: container }",
                "  node-b: { role: ecu, zone: front-left, backend: qemu-microvm }",
            );
        let model = crate::model::load_yaml(&ws).expect("valid test workspace");
        let gw = GatewayPlan::from_model(&model).unwrap();
        assert!(matches!(
            plan_nodes_from_model(&model, &gw, Path::new("/x")),
            Err(ZonePlanError::MissingBundle(id, _)) if id == "node-b"
        ));
    }

    /// Two bootable parts on one device is not a plan a hypervisor can execute, and choosing
    /// one silently would boot half the arm.
    #[test]
    fn a_device_hosting_two_bundles_is_refused() {
        let ws = WS
            .replace(
                "  fn-b: { deployed_on: node-b }",
                "  fn-b: { deployed_on: node-b }\n  fn-a2: { deployed_on: node-a }",
            )
            .replace(
                "  - { ref: 'local/vxprobe:host-process', target_ids: fn-b }",
                "  - { ref: 'local/vxprobe:host-process', target_ids: fn-b }\n  - { ref: ./other, kind: bundle, target_ids: fn-a2 }",
            );
        let model = crate::model::load_yaml(&ws).expect("valid test workspace");
        let gw = GatewayPlan::from_model(&model).unwrap();
        assert!(matches!(
            plan_nodes_from_model(&model, &gw, Path::new("/x")),
            Err(ZonePlanError::AmbiguousBundle { device, count: 2 }) if device == "node-a"
        ));
    }
}
