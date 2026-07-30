//! Build the domain model from a loaded §6.0 workspace.
//!
//! **Transitional shape.** §6.0's YAML still says `composition: {zones, compute, gateway}` —
//! components grouped by what they are, with software attached straight to them. The model says
//! zone/device/function. So this builder maps the former onto the latter, synthesizing one
//! [`Function`] per device from the part targeting it.
//!
//! That synthesis is deliberate and temporary. It means every planner can move onto the model
//! *now*, against workspaces that already exist, before the schema grows `zones:`/`devices:`/
//! `functions:` blocks of its own. When it does, only this file changes — which is the point of
//! having the model be a separate type rather than the parsed YAML.
//!
//! The reverse direction (§6.1 → model) is the adapter that keeps the frozen experiments
//! runnable; it lands with task 5c.

use super::*;
use crate::workspace::{PartEntry, VehicleDoc, Workspace};
use serde_yaml_ng::Value;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("workspace has no `kind: experiment` document")]
    NoExperiment,
    #[error("experiment `{experiment}` names no vehicle")]
    NoEntity { experiment: String },
    #[error(
        "experiment `{experiment}` references vehicle `{entity}`, which is not in this workspace"
    )]
    UnknownEntity { experiment: String, entity: String },
    #[error(
        "component `{component}` has {count} parts. A device runs one loadable; publish them as \
         a single `stack`-layer composite and reference that."
    )]
    LayeredStack { component: String, count: usize },
    #[error("component `{component}` has no part — nothing to run on it")]
    NoPart { component: String },
    #[error(
        "device `{device}` declares backend `{backend}`, which is not container, qemu-microvm or libvirt"
    )]
    UnknownBackend { device: String, backend: String },
}

fn str_of<'a>(spec: &'a Value, key: &str) -> Option<&'a str> {
    spec.get(key).and_then(|v| v.as_str())
}

fn backend_of(device: &str, spec: &Value) -> Result<Option<Backend>, ModelError> {
    match str_of(spec, "backend") {
        None => Ok(None),
        Some("container") => Ok(Some(Backend::Container)),
        Some("qemu-microvm") => Ok(Some(Backend::QemuMicrovm)),
        Some("libvirt") => Ok(Some(Backend::Libvirt)),
        Some(other) => Err(ModelError::UnknownBackend {
            device: device.to_string(),
            backend: other.to_string(),
        }),
    }
}

fn resources_of(spec: &Value) -> Option<Resources> {
    let r = spec.get("resources")?;
    Some(Resources {
        cpu: r.get("cpu").and_then(|v| v.as_u64()).map(|v| v as u32),
        memory_gb: r
            .get("memory")
            .or_else(|| r.get("memory_gb"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        gpu: r.get("gpu").and_then(|v| v.as_str()).map(str::to_owned),
    })
}

/// The one part targeting `component`. A `stack`-layer part wins over its own members: a stack
/// exists precisely to be the single pullable unit, so carrying both is not ambiguous.
fn sole_part(ws: &Workspace, component: &str) -> Result<Part, ModelError> {
    let candidates: Vec<&PartEntry> = ws
        .baselines
        .iter()
        .flat_map(|b| b.parts.iter())
        .filter(|p| {
            p.target_ids
                .as_ref()
                .is_some_and(|t| t.as_slice().contains(&component))
        })
        .collect();

    let chosen = match candidates.len() {
        0 => {
            return Err(ModelError::NoPart {
                component: component.to_string(),
            });
        }
        1 => candidates[0],
        n => {
            let stacks: Vec<&&PartEntry> = candidates
                .iter()
                .filter(|p| p.reference.contains("/stack/"))
                .collect();
            if stacks.len() == 1 {
                stacks[0]
            } else {
                return Err(ModelError::LayeredStack {
                    component: component.to_string(),
                    count: n,
                });
            }
        }
    };
    Ok(Part {
        reference: chosen.reference.clone(),
        digest: chosen.digest.clone(),
        local: chosen.is_local(),
    })
}

fn endpoint(raw: &str) -> Endpoint {
    match raw.split_once(':') {
        Some((n, p)) => Endpoint {
            node: n.to_string(),
            port: Some(p.to_string()),
            ip_cidr: None,
        },
        None => Endpoint {
            node: raw.to_string(),
            port: None,
            ip_cidr: None,
        },
    }
}

fn link_kind(slice: Option<&String>) -> LinkKind {
    // Today's §6.0 links carry only a slice; a slice-bound link crosses the gateway trunk,
    // anything else is a direct attachment. Explicit `kind:` arrives with the schema change.
    match slice {
        Some(_) => LinkKind::VlanTrunk,
        None => LinkKind::Direct,
    }
}

fn build_entity(ws: &Workspace, doc: &VehicleDoc) -> Result<Entity, ModelError> {
    let mut zones: BTreeMap<String, Zone> = BTreeMap::new();
    let mut devices: BTreeMap<String, Device> = BTreeMap::new();
    let mut functions: BTreeMap<String, Function> = BTreeMap::new();

    // Three §6.0 groups collapse into one device map keyed by role. `position` becomes a *zone*
    // the device sits in, rather than the device's own identity — the conflation this model
    // exists to undo.
    let groups: [(&BTreeMap<String, Value>, DeviceRole); 3] = [
        (&doc.composition.zones, DeviceRole::ZoneController),
        (&doc.composition.compute, DeviceRole::Hpc),
        (&doc.composition.gateway, DeviceRole::Gateway),
    ];

    for (group, role) in groups {
        for (id, spec) in group {
            let zone = str_of(spec, "position").map(str::to_owned);
            if let Some(z) = &zone {
                zones.entry(z.clone()).or_insert_with(|| Zone {
                    id: z.clone(),
                    name: None,
                    reference: None,
                });
            }
            devices.insert(
                id.clone(),
                Device {
                    id: id.clone(),
                    role,
                    zone,
                    resources: resources_of(spec),
                    backend: backend_of(id, spec)?,
                    signals: Vec::new(),
                    pinned_cpus: spec
                        .get("realtime")
                        .and_then(|r| r.get("pinned_cpus"))
                        .and_then(|v| v.as_sequence())
                        .map(|seq| {
                            seq.iter()
                                .filter_map(|v| v.as_u64())
                                .map(|v| v as u32)
                                .collect()
                        })
                        .unwrap_or_default(),
                },
            );

            // One synthesized function per device, named after it. §6.0 attaches software to a
            // component, so the function layer has no separate identity yet — this keeps
            // channels and parts resolvable through the model without inventing names an
            // author never wrote and could not refer to.
            functions.insert(
                id.clone(),
                Function {
                    id: id.clone(),
                    deployed_on: id.clone(),
                    part: Some(sole_part(ws, id)?),
                    asil: None,
                },
            );
        }
    }

    let links = doc
        .links
        .iter()
        .map(|l| Link {
            id: l.id.clone(),
            endpoints: l.endpoints.iter().map(|e| endpoint(e)).collect(),
            kind: link_kind(l.slice.as_ref()),
            slice: l.slice.clone(),
            harness: None,
        })
        .collect();

    let channels = doc
        .channels
        .iter()
        .map(|c| Channel {
            id: c.id.clone(),
            from: endpoint(&c.from),
            to: c.to.iter().map(|e| endpoint(e)).collect(),
            // §6.0 carries `protocol:` as a string today; anything unrecognised falls back to
            // UDP rather than failing, because the interaction plane is not yet executed and a
            // hard error here would reject workspaces over a field nothing reads.
            protocol: Protocol::Udp,
            qos: None,
            carried_by: c.carried_by.clone(),
        })
        .collect();

    Ok(Entity {
        id: doc.id.clone(),
        class: EntityClass::Road,
        subtype: None,
        zones,
        devices,
        functions,
        links,
        channels,
    })
}

fn slices_of(raw: &Value) -> Vec<Slice> {
    let Some(list) = raw
        .get("network")
        .and_then(|n| n.get("slices"))
        .and_then(|s| s.as_sequence())
    else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|s| {
            let id = s.get("id")?.as_str()?.to_string();
            let mode = match s.get("mode").and_then(|m| m.as_str()) {
                Some("s-nssai") => SliceMode::SNssai,
                _ => SliceMode::Vlan,
            };
            let qos = s.get("qos").map(|q| SliceQos {
                min_rate_mbps: q
                    .get("min_rate_mbps")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32),
                max_rate_mbps: q
                    .get("max_rate_mbps")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32),
                priority: q.get("priority").and_then(|v| v.as_u64()).map(|v| v as u8),
            });
            Some(Slice {
                id,
                mode,
                vlan: s.get("vlan").and_then(|v| v.as_u64()).map(|v| v as u16),
                s_nssai: s.get("s_nssai").and_then(|n| {
                    Some(SNssai {
                        sst: n.get("sst")?.as_u64()? as u8,
                        sd: n.get("sd").and_then(|v| v.as_str()).map(str::to_owned),
                    })
                }),
                qos,
            })
        })
        .collect()
}

fn traffic_endpoint(raw: &str, devices: &BTreeMap<String, Device>) -> TrafficEndpoint {
    if devices.contains_key(raw) {
        TrafficEndpoint::Device(raw.to_string())
    } else if raw == "internet" {
        TrafficEndpoint::Internet
    } else {
        TrafficEndpoint::Simulator(raw.to_string())
    }
}

/// Build the model from a **validated** workspace.
///
/// Validation stays in [`crate::Workspace::validate`] rather than moving here: a cross-file
/// reference error should be reported against the file the author wrote, not against a derived
/// structure they have never seen.
pub fn build(ws: &Workspace) -> Result<Experiment, ModelError> {
    let exp = ws.experiments.first().ok_or(ModelError::NoExperiment)?;
    let raw = ws.experiments_raw.first().cloned().unwrap_or(Value::Null);

    let mut entities = Vec::new();
    for id in &exp.vehicles {
        let doc =
            ws.vehicles
                .iter()
                .find(|v| &v.id == id)
                .ok_or_else(|| ModelError::UnknownEntity {
                    experiment: exp.id.clone(),
                    entity: id.clone(),
                })?;
        entities.push(build_entity(ws, doc)?);
    }
    if entities.is_empty() {
        return Err(ModelError::NoEntity {
            experiment: exp.id.clone(),
        });
    }

    let devices = &entities[0].devices;
    let traffic = raw
        .get("traffic_profiles")
        .and_then(|t| t.as_mapping())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    Some(TrafficProfile {
                        id: k.as_str()?.to_string(),
                        source: traffic_endpoint(v.get("source")?.as_str()?, devices),
                        target: traffic_endpoint(v.get("target")?.as_str()?, devices),
                        pps: v.get("pps").and_then(|x| x.as_u64()).map(|x| x as u32),
                        bandwidth_mbps: v
                            .get("bandwidth_mbps")
                            .and_then(|x| x.as_u64())
                            .map(|x| x as u32),
                        payload_bytes: v
                            .get("payload_bytes")
                            .and_then(|x| x.as_u64())
                            .map(|x| x as u32),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let method = raw
        .get("method")
        .map(|m| Method {
            duration_s: m
                .get("duration_s")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            repetitions: m
                .get("repetitions")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32),
            warmup_s: m.get("warmup_s").and_then(|v| v.as_u64()).map(|v| v as u32),
        })
        .unwrap_or_default();

    let mut passthrough = BTreeMap::new();
    for key in ["services", "telemetry", "results"] {
        if let Some(v) = raw.get(key) {
            passthrough.insert(key.to_string(), v.clone());
        }
    }

    Ok(Experiment {
        id: exp.id.clone(),
        uplink_bandwidth_bps: raw
            .get("network")
            .and_then(|n| n.get("defaults"))
            .and_then(|d| d.get("bandwidth"))
            .and_then(|v| v.as_u64()),
        slices: slices_of(&raw),
        entities,
        traffic,
        method,
        passthrough,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::{Template, WorkspacePlan};

    fn scaffolded(name: &str) -> (std::path::PathBuf, Workspace) {
        let tmp = std::env::temp_dir().join(format!("vxmodel-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let root = WorkspacePlan::plan("demo", Template::Slicing)
            .unwrap()
            .write(&tmp)
            .unwrap();
        let ws = Workspace::load(&root).unwrap();
        (tmp, ws)
    }

    /// The conflation the model exists to undo: `position` is a *zone the device is in*, not the
    /// device's identity, and the three §6.0 groups are all devices differing only by role.
    #[test]
    fn components_become_devices_located_in_zones() {
        let (tmp, ws) = scaffolded("zones");
        let m = build(&ws).unwrap();
        let e = &m.entities[0];

        assert_eq!(e.device("zonefl").unwrap().role, DeviceRole::ZoneController);
        assert_eq!(e.device("hpc1").unwrap().role, DeviceRole::Hpc);
        assert_eq!(e.device("gw1").unwrap().role, DeviceRole::Gateway);

        assert_eq!(
            e.device("zonefl").unwrap().zone.as_deref(),
            Some("front-left")
        );
        assert!(e.zones.contains_key("front-left"), "{:?}", e.zones.keys());
        // The gateway belongs to the entity, not to a region.
        assert_eq!(e.device("gw1").unwrap().zone, None);
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Software attaches to a function, and the function is deployed onto a device. Even while
    /// the §6.0 schema still writes them as one thing, the model keeps them distinct — which is
    /// what a channel endpoint will resolve through.
    #[test]
    fn software_reaches_a_device_through_a_function() {
        let (tmp, ws) = scaffolded("fn");
        let m = build(&ws).unwrap();
        let e = &m.entities[0];
        let f = e.functions.get("zonefl").expect("function synthesized");
        assert_eq!(f.deployed_on, "zonefl");
        assert!(f.part.as_ref().unwrap().reference.contains("probe-bundle"));
        assert_eq!(e.host_of("zonefl").unwrap().id, "zonefl");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn slices_links_and_traffic_survive() {
        let (tmp, ws) = scaffolded("net");
        let m = build(&ws).unwrap();
        assert!(
            m.slices
                .iter()
                .any(|s| s.id == "adas" && s.vlan == Some(101))
        );
        let e = &m.entities[0];
        assert!(e.links.iter().any(|l| l.slice.as_deref() == Some("adas")));
        assert!(
            m.traffic
                .iter()
                .any(|t| t.source == TrafficEndpoint::Device("zonefl".into())
                    && t.target == TrafficEndpoint::Internet)
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// 5b's acceptance criterion: the model path and the §6.1 path must produce the **same**
    /// `GatewayPlan`. Endpoint names are the sharp edge — campaign scripts address `vzp0`/`vzp1`
    /// directly — so "both plan successfully" is not enough; they have to plan identically.
    ///
    /// Fixture mirrors exp-001: two container-backed devices on separate vlan slices.
    /// exp-001 in both forms: two container-backed devices on separate vlan slices, over a
    /// 500 mbit shared bottleneck. Returns (dir, §6.1 manifest, §6.0 workspace).
    fn equivalence_fixture(
        name: &str,
    ) -> (std::path::PathBuf, crate::ExperimentManifest, Workspace) {
        let frozen = crate::ExperimentManifest::from_yaml(
            r#"
experiment: { id: exp-x, name: x, duration_s: 60 }
network:
  defaults: { bandwidth: 500000000 }
  slices:
    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }
    - { id: infot, mode: vlan, vlan: 103, qos: { min_rate_mbps: 0, max_rate_mbps: 400, priority: 5 } }
zones:
  - id: vxzone-a
    role: front-left
    nodes:
      - id: node-a
        type: ecu
        image: { url: "local/vxprobe:host-process", kind: container }
        backend: container
        network: [{ iface: eth0, vlan: 101, slice: adas }]
  - id: vxzone-b
    role: rear-left
    nodes:
      - id: node-b
        type: ecu
        image: { url: "local/vxtraffic:host-process", kind: container }
        backend: container
        network: [{ iface: eth0, vlan: 103, slice: infot }]
traffic_profiles:
  load:
    source: vxzone-b:node-b
    target: internet
    bandwidth_mbps: 400
    payload_bytes: 1200
"#,
        )
        .unwrap();

        let tmp = std::env::temp_dir().join(format!("vxmodelplan-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("x.vxp"), "id: exp-x\n").unwrap();
        std::fs::write(
            tmp.join("main.yaml"),
            r#"
kind: experiment
id: exp-x
vehicles: [veh]
baseline: bl
network:
  defaults: { bandwidth: 500000000 }
  slices:
    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }
    - { id: infot, mode: vlan, vlan: 103, qos: { min_rate_mbps: 0, max_rate_mbps: 400, priority: 5 } }
traffic_profiles:
  load:
    source: node-b
    target: internet
    bandwidth_mbps: 400
    payload_bytes: 1200
"#,
        )
        .unwrap();
        std::fs::write(
            tmp.join("veh.yaml"),
            r#"
kind: vehicle
id: veh
composition:
  zones:
    node-a: { class: zone, position: front-left, backend: container }
    node-b: { class: zone, position: rear-left, backend: container }
links:
  - { id: la, endpoints: [node-a:eth0], slice: adas }
  - { id: lb, endpoints: [node-b:eth0], slice: infot }
"#,
        )
        .unwrap();
        std::fs::write(
            tmp.join("parts.yaml"),
            r#"
kind: baseline
id: bl
parts:
  - { ref: "local/vxprobe:host-process", target_ids: node-a }
  - { ref: "local/vxtraffic:host-process", target_ids: node-b }
"#,
        )
        .unwrap();

        let ws = Workspace::load_validated(&tmp).unwrap();
        (tmp, frozen, ws)
    }

    #[test]
    fn the_model_path_plans_the_same_gateway_as_the_manifest_path() {
        use crate::gateway::GatewayPlan;
        let (tmp, frozen, ws) = equivalence_fixture("gw");
        let via_model = GatewayPlan::from_model(&build(&ws).unwrap()).unwrap();
        let via_manifest = GatewayPlan::from_manifest(&frozen).unwrap();

        assert_eq!(via_model.summary(), via_manifest.summary());
        assert_eq!(via_model.endpoints, via_manifest.endpoints);
        // The shared bottleneck the whole experiment measures against. Asserted explicitly
        // because `summary()` including it is not something this test should depend on.
        assert_eq!(via_model.root_rate_mbps, 500);
        assert_eq!(via_model.root_rate_mbps, via_manifest.root_rate_mbps);
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Same acceptance criterion as the gateway port: the two paths must produce the *same*
    /// node plans, since `NodePlan` carries the interface names the backends attach to.
    ///
    /// `zone_id` is the one field that legitimately differs — §6.1 names a wrapper zone, the
    /// model names the region the device sits in. Everything the provisioning actually uses
    /// (endpoint, peer, kind, vlan, backend) must be identical.
    #[test]
    fn the_model_path_plans_the_same_nodes_as_the_manifest_path() {
        use crate::gateway::GatewayPlan;
        use crate::zones::plan::{plan_nodes, plan_nodes_from_model};

        let (tmp, frozen, ws) = equivalence_fixture("nodes");
        let model = build(&ws).unwrap();

        let gw_model = GatewayPlan::from_model(&model).unwrap();
        let gw_manifest = GatewayPlan::from_manifest(&frozen).unwrap();
        let via_model = plan_nodes_from_model(&model, &gw_model, &tmp).unwrap();
        let via_manifest = plan_nodes(&frozen, &gw_manifest, &tmp).unwrap();

        assert_eq!(via_model.len(), via_manifest.len());
        for (a, b) in via_model.iter().zip(via_manifest.iter()) {
            assert_eq!(a.node_id, b.node_id);
            assert_eq!(a.endpoint, b.endpoint, "attachment name drifted");
            assert_eq!(a.peer, b.peer, "veth peer drifted");
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.vlan, b.vlan);
            assert_eq!(a.backend, b.backend);
            assert_eq!(a.runtime_name(), b.runtime_name(), "netns name drifted");
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Third planner ported. Traffic is where §6.1's endpoint spelling was worst — a profile
    /// names `<zone-vxid>:<node-vxid>`, and the zone half was synthesized by lowering, so no
    /// author could ever write it correctly. In the model an endpoint is a device id, and the
    /// resolution happens once at build time.
    #[test]
    fn the_model_path_plans_the_same_traffic_as_the_manifest_path() {
        use crate::gateway::GatewayPlan;
        use crate::traffic::TrafficPlan;

        let (tmp, frozen, ws) = equivalence_fixture("traffic");
        let model = build(&ws).unwrap();

        let via_model = TrafficPlan::from_model(&model, &GatewayPlan::from_model(&model).unwrap())
            .expect("model traffic plans");
        let via_manifest =
            TrafficPlan::from_manifest(&frozen, &GatewayPlan::from_manifest(&frozen).unwrap())
                .expect("manifest traffic plans");

        assert_eq!(via_model.profiles.len(), via_manifest.profiles.len());
        for (a, b) in via_model.profiles.iter().zip(via_manifest.profiles.iter()) {
            assert_eq!(a.id, b.id);
            // The resolved endpoints carry the interface and address traffic is actually
            // generated against — the whole point of the comparison.
            assert_eq!(a.source, b.source, "source endpoint drifted");
            assert_eq!(a.target, b.target, "target endpoint drifted");
            assert_eq!(a.bandwidth_mbps, b.bandwidth_mbps);
            assert_eq!(a.payload_bytes, b.payload_bytes);
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Every scaffolded template must satisfy every model invariant. If the generator can emit
    /// something the model rejects, one of the two is wrong.
    #[test]
    fn scaffolded_workspaces_satisfy_every_model_invariant() {
        for template in [Template::Minimal, Template::Slicing] {
            let tmp = std::env::temp_dir().join(format!(
                "vxinv-{}-{}",
                std::process::id(),
                template.as_str()
            ));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp).unwrap();
            let root = WorkspacePlan::plan("demo", template)
                .unwrap()
                .write(&tmp)
                .unwrap();
            let ws = Workspace::load(&root).unwrap();
            let errors = build(&ws).unwrap().check();
            assert!(errors.is_empty(), "{}: {errors:?}", template.as_str());
            std::fs::remove_dir_all(&tmp).ok();
        }
    }

    /// The collision the new id spaces make possible: §6.0 derives a zone id from `position` and
    /// a device id from the component key, so a component named after its own region produces
    /// two different things answering to one name.
    #[test]
    fn a_device_named_after_its_zone_is_a_collision() {
        let (tmp, ws) = scaffolded("collide");
        let mut m = build(&ws).unwrap();
        let e = &mut m.entities[0];
        let mut d = e.devices.get("zonefl").unwrap().clone();
        d.id = "front-left".into();
        e.devices.insert("front-left".into(), d);
        assert!(
            m.check()
                .iter()
                .any(|e| matches!(e, crate::model::InvariantError::ZoneDeviceCollision { .. })),
            "{:?}",
            m.check()
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Channels connect functions, not devices. That is the distinction the model exists to
    /// make, so pointing one at a device has to be an error rather than a silent near-miss.
    #[test]
    fn a_channel_pointed_at_a_non_function_is_caught() {
        let (tmp, ws) = scaffolded("chan");
        let mut m = build(&ws).unwrap();
        m.entities[0].channels.push(crate::model::Channel {
            id: "bogus".into(),
            from: crate::model::Endpoint {
                node: "not-a-function".into(),
                ..Default::default()
            },
            to: vec![],
            protocol: crate::model::Protocol::Udp,
            qos: None,
            carried_by: vec!["also-not-a-link".into()],
        });
        let errors = m.check();
        assert!(errors.iter().any(|e| matches!(
            e,
            crate::model::InvariantError::UnknownChannelEndpoint { .. }
        )));
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, crate::model::InvariantError::UnknownCarriedByLink { .. }))
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A device the planner cannot realise is refused with the fix in the message rather than
    /// defaulted — the guess is exactly what produced a workspace that validated and then could
    /// not boot during the exp-001 migration.
    #[test]
    fn a_slice_attached_device_without_a_backend_is_refused() {
        use crate::gateway::GatewayPlan;
        let (tmp, ws) = scaffolded("nobackend");
        let m = build(&ws).unwrap();
        match GatewayPlan::from_model(&m) {
            Err(crate::gateway::plan::PlanError::DeviceWithoutBackend(d)) => {
                assert!(!d.is_empty())
            }
            other => panic!("expected DeviceWithoutBackend, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_backend_the_testbed_cannot_run_is_refused() {
        let (tmp, mut ws) = scaffolded("badbackend");
        ws.vehicles[0]
            .composition
            .zones
            .get_mut("zonefl")
            .unwrap()
            .as_mapping_mut()
            .unwrap()
            .insert(Value::String("backend".into()), Value::String("xen".into()));
        match build(&ws) {
            Err(ModelError::UnknownBackend { backend, .. }) => assert_eq!(backend, "xen"),
            other => panic!("expected UnknownBackend, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
