//! §6.1 → model. The adapter that keeps frozen experiments runnable during the migration.
//!
//! §6.1 is rejected as an authoring schema, but deleting it the moment the model exists would
//! delete the baseline the migration is being checked against: exp-001/002/002b/003/010 have
//! recorded numbers, and "the model reproduces them" is the only evidence that rejecting §6.1
//! lost nothing. So §6.1 stops being a *planner input* here and becomes an input *format*, with
//! exactly one way in. It goes entirely at task 5f, once those runs pass.
//!
//! # The ordering hazard this exists to contain
//!
//! Endpoint names are positional — `vz0`, `vz1`, … — and campaign scripts address them
//! directly. §6.1 declares zones and nodes as *sequences*, so its plans were built from
//! declaration order. §6.0 stores components in a *map*, so sorted id is the only order
//! available there. Those disagree: exp-002 declares `cont, microvm, libvirt` and sorts to
//! `cont, libvirt, microvm`, so feeding a sorted model into the planner would have silently
//! swapped two H3 arms — the campaign would have measured libvirt and reported microvm.
//!
//! The adapter therefore preserves declaration order in `device_order` rather than letting the
//! model re-derive it.

use super::*;
use crate::manifest::{Backend as MBackend, ExperimentManifest, ZoneRole};

/// A §6.1 concept the model has no place for.
#[derive(Debug, thiserror::Error)]
pub enum AdaptError {
    #[error("node `{0}` declares `slice_policy`, which the model does not carry yet")]
    SlicePolicy(String),
}

fn role_of(zone_role: ZoneRole) -> (DeviceRole, Option<String>) {
    // §6.1's ZoneRole conflates three different node kinds into one enum — the conflation §6.0
    // was created to split. Undoing it here is what makes a frozen manifest expressible in the
    // model at all.
    match zone_role {
        ZoneRole::Gateway => (DeviceRole::Gateway, None),
        ZoneRole::CentralHpc => (DeviceRole::Hpc, None),
        // Positional roles are genuinely regions, so they become the device's zone.
        other => (
            DeviceRole::ZoneController,
            Some(format!("{other:?}").to_lowercase()),
        ),
    }
}

fn backend_of(b: Option<MBackend>) -> Option<Backend> {
    b.map(|b| match b {
        MBackend::Container => Backend::Container,
        MBackend::Libvirt => Backend::Libvirt,
        MBackend::QemuMicrovm => Backend::QemuMicrovm,
    })
}

impl ExperimentManifest {
    /// Build the domain model from a §6.1 manifest.
    ///
    /// The whole manifest becomes one entity: §6.1 has no notion of multiple vehicles, and
    /// inventing a split here would be modelling something the source document does not say.
    pub fn to_model(&self) -> Result<Experiment, AdaptError> {
        let mut zones: BTreeMap<String, Zone> = BTreeMap::new();
        let mut devices: BTreeMap<String, Device> = BTreeMap::new();
        let mut device_order: Vec<String> = Vec::new();
        let mut functions: BTreeMap<String, Function> = BTreeMap::new();
        let mut links: Vec<Link> = Vec::new();

        for zone in &self.zones {
            let (role, region) = role_of(zone.role);
            if let Some(r) = &region {
                zones.entry(r.clone()).or_insert_with(|| Zone {
                    id: r.clone(),
                    name: zone.name.clone(),
                    reference: None,
                });
            }
            for node in &zone.nodes {
                if !node.slice_policy.is_empty() {
                    return Err(AdaptError::SlicePolicy(node.id.clone()));
                }
                // The backend §6.1 would have resolved. Made explicit here because the model
                // refuses to guess how a device is realised.
                let backend = backend_of(node.backend).unwrap_or(match node.image.kind {
                    crate::manifest::ImageKind::Container => Backend::Container,
                    _ => Backend::Libvirt,
                });
                devices.insert(
                    node.id.clone(),
                    Device {
                        id: node.id.clone(),
                        role,
                        zone: region.clone(),
                        resources: None,
                        backend: Some(backend),
                        signals: Vec::new(),
                        pinned_cpus: node
                            .realtime
                            .as_ref()
                            .map(|r| r.pinned_cpus.clone())
                            .unwrap_or_default(),
                    },
                );
                device_order.push(node.id.clone());
                functions.insert(
                    node.id.clone(),
                    Function {
                        id: node.id.clone(),
                        deployed_on: node.id.clone(),
                        part: Some(Part {
                            reference: node.image.url.clone(),
                            digest: None,
                            local: true,
                        }),
                        asil: None,
                    },
                );
                // One link per gateway-attached iface, in iface order — this is what preserves
                // §6.1's endpoint numbering exactly.
                for iface in &node.network {
                    if iface.vlan.is_none() {
                        continue;
                    }
                    links.push(Link {
                        id: format!("{}-{}", node.id, iface.iface),
                        endpoints: vec![Endpoint {
                            node: node.id.clone(),
                            port: Some(iface.iface.clone()),
                            ip_cidr: iface.ip.clone(),
                        }],
                        kind: LinkKind::VlanTrunk,
                        slice: iface.slice.clone(),
                        harness: None,
                    });
                }
            }
        }

        for c in &self.compute {
            devices.insert(
                c.id.clone(),
                Device {
                    id: c.id.clone(),
                    role: DeviceRole::Hpc,
                    zone: None,
                    resources: c.resources.as_ref().map(|r| Resources {
                        cpu: r.cpu,
                        memory_gb: r.memory,
                        gpu: r.gpu.as_ref().map(|g| format!("{g:?}").to_lowercase()),
                    }),
                    backend: Some(Backend::Container),
                    signals: Vec::new(),
                    pinned_cpus: Vec::new(),
                },
            );
            device_order.push(c.id.clone());
            functions.insert(
                c.id.clone(),
                Function {
                    id: c.id.clone(),
                    deployed_on: c.id.clone(),
                    part: Some(Part {
                        reference: c.image.url.clone(),
                        digest: None,
                        local: true,
                    }),
                    asil: None,
                },
            );
        }

        let network = self.network.as_ref();
        let slices = network
            .map(|n| {
                n.slices
                    .iter()
                    .map(|s| Slice {
                        id: s.id.clone(),
                        mode: match s.mode {
                            crate::manifest::SliceMode::Vlan => SliceMode::Vlan,
                            crate::manifest::SliceMode::Snssai => SliceMode::SNssai,
                        },
                        vlan: s.vlan,
                        s_nssai: s.s_nssai.as_ref().map(|n| SNssai {
                            sst: n.sst,
                            sd: n.sd.clone(),
                        }),
                        qos: s.qos.as_ref().map(|q| SliceQos {
                            min_rate_mbps: q.min_rate_mbps.map(|v| v as u32),
                            max_rate_mbps: q.max_rate_mbps.map(|v| v as u32),
                            priority: q.priority,
                        }),
                    })
                    .collect()
            })
            .unwrap_or_default();

        // §6.1 spells a traffic endpoint `<zone-vxid>:<node-vxid>`; the model wants the device.
        let to_endpoint = |raw: &str| -> TrafficEndpoint {
            let node = raw.split_once(':').map(|(_, n)| n).unwrap_or(raw);
            if devices.contains_key(node) {
                TrafficEndpoint::Device(node.to_string())
            } else if raw == "internet" {
                TrafficEndpoint::Internet
            } else {
                TrafficEndpoint::Simulator(raw.to_string())
            }
        };
        let traffic = self
            .traffic_profiles
            .iter()
            .map(|(id, p)| TrafficProfile {
                id: id.clone(),
                source: to_endpoint(&p.source),
                target: to_endpoint(&p.target),
                pps: p.pps.map(|v| v as u32),
                bandwidth_mbps: p.bandwidth_mbps.map(|v| v as u32),
                payload_bytes: p.payload_bytes,
            })
            .collect();

        Ok(Experiment {
            id: self.experiment.id.clone(),
            uplink_bandwidth_bps: network
                .and_then(|n| n.defaults.as_ref())
                .and_then(|d| d.bandwidth),
            entities: vec![Entity {
                id: self.experiment.id.clone(),
                class: EntityClass::Road,
                subtype: None,
                zones,
                device_order,
                devices,
                functions,
                links,
                channels: Vec::new(),
            }],
            slices,
            traffic,
            method: Method {
                duration_s: self
                    .experiment
                    .duration_s
                    .and_then(|v| u32::try_from(v).ok()),
                repetitions: None,
                warmup_s: None,
            },
            passthrough: BTreeMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::ExperimentManifest;
    use crate::gateway::GatewayPlan;

    /// The hazard that made explicit `device_order` necessary, in the shape exp-002 actually has:
    /// declaration order and sorted order disagree. If the model sorted, `vz1` and `vz2` would
    /// swap, each H3 arm would attach to the other's interface, and the campaign would report
    /// microvm numbers measured on libvirt — passing every test and every validation.
    #[test]
    fn declaration_order_survives_the_adapter_even_when_it_is_not_sorted() {
        let m = ExperimentManifest::from_yaml(
            r#"
experiment: { id: exp-h3, name: h3, duration_s: 60 }
network:
  slices:
    - { id: s, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, priority: 1 } }
zones:
  - id: z
    role: front-left
    nodes:
      - id: node-cont-01
        type: ecu
        image: { url: "local/x:1", kind: container }
        backend: container
        network: [{ iface: eth0, vlan: 101, slice: s }]
      - id: node-microvm-01
        type: ecu
        image: { url: "./b", kind: rtimage }
        backend: qemu-microvm
        network: [{ iface: eth0, vlan: 101, slice: s }]
      - id: node-libvirt-01
        type: ecu
        image: { url: "./b", kind: rtimage }
        backend: libvirt
        network: [{ iface: eth0, vlan: 101, slice: s }]
"#,
        )
        .unwrap();

        let model = m.to_model().unwrap();
        assert_eq!(
            model.entities[0].device_order,
            vec!["node-cont-01", "node-microvm-01", "node-libvirt-01"],
            "declaration order lost — sorted order would swap the microvm and libvirt arms"
        );

        let via_model = GatewayPlan::from_model(&model).unwrap();
        let via_manifest = GatewayPlan::from_manifest(&m).unwrap();
        assert_eq!(via_model.endpoints, via_manifest.endpoints);
        // Spelled out, because this is the pairing a campaign script hardcodes. Note the peer
        // name differs by backend: a container gets a veth (`vzp1`), a VM gets a tap whose peer
        // *is* the endpoint (`vz1`) — so the arms differ in more than position and swapping them
        // would not even be caught by an interface-exists check.
        assert_eq!(via_model.endpoints[0].node, "node-cont-01");
        assert_eq!(via_model.endpoints[0].peer, "vzp0");
        assert_eq!(via_model.endpoints[1].node, "node-microvm-01");
        assert_eq!(via_model.endpoints[1].peer, "vz1");
        assert_eq!(via_model.endpoints[2].node, "node-libvirt-01");
        assert_eq!(via_model.endpoints[2].peer, "vz2");
    }

    /// §6.1's ZoneRole conflates three node kinds into one enum. Undoing that split is what
    /// makes a frozen manifest expressible in the model at all.
    #[test]
    fn zone_roles_split_into_role_plus_region() {
        let m = ExperimentManifest::from_yaml(
            r#"
experiment: { id: e, name: e }
network: { slices: [{ id: s, mode: vlan, vlan: 101 }] }
zones:
  - id: z1
    role: front-left
    nodes: [{ id: n1, type: ecu, image: { url: "local/x:1", kind: container } }]
  - id: z2
    role: gateway
    nodes: [{ id: n2, type: gateway, image: { url: "local/x:1", kind: container } }]
  - id: z3
    role: central-hpc
    nodes: [{ id: n3, type: ecu, image: { url: "local/x:1", kind: container } }]
"#,
        )
        .unwrap();
        let model = m.to_model().unwrap();
        let e = &model.entities[0];
        use crate::model::DeviceRole;
        assert_eq!(e.devices["n1"].role, DeviceRole::ZoneController);
        assert_eq!(e.devices["n1"].zone.as_deref(), Some("frontleft"));
        assert_eq!(e.devices["n2"].role, DeviceRole::Gateway);
        assert_eq!(e.devices["n2"].zone, None, "a gateway is not a region");
        assert_eq!(e.devices["n3"].role, DeviceRole::Hpc);
        assert_eq!(e.devices["n3"].zone, None);
        assert!(e.zones.contains_key("frontleft"), "{:?}", e.zones.keys());
        assert!(model.check().is_empty(), "{:?}", model.check());
    }
}
