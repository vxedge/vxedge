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
        "`{component}`: part `{part}` is a {kind}, but the device it deploys onto runs the \
         `{backend}` backend. A container backend boots a container image; libvirt and \
         qemu-microvm boot a guest bundle. Point this target at the right part, or correct the \
         device's `backend`."
    )]
    PartKindMismatch {
        component: String,
        part: String,
        kind: &'static str,
        backend: &'static str,
    },
    #[error("`{component}`: part `{part}` declares `kind: {kind}` — expected container or bundle")]
    UnknownPartKind {
        component: String,
        part: String,
        kind: String,
    },
    #[error(
        "device `{device}` declares backend `{backend}`, which is not container, qemu-microvm or libvirt"
    )]
    UnknownBackend { device: String, backend: String },
    #[error(
        "device `{device}` declares role `{role}` — expected zone-controller, hpc, gateway, ecu, \
         sensor, actuator or switch"
    )]
    UnknownRole { device: String, role: String },
    #[error("`{field}` is {value}, which does not fit a {ty} (max {max})")]
    OutOfRange {
        field: String,
        value: u64,
        ty: &'static str,
        max: u64,
    },
}

/// Range-checked narrowing from YAML's `u64`.
///
/// §6.1 got this for free: serde types the field `u16`/`u8` and rejects anything larger. The
/// model reads `as_u64()` and used to cast, which silently wraps — `vlan: 70000` became VLAN
/// 4464 and provisioned the wrong slice with nothing reported. Narrowing has to be checked, so
/// it is checked in one place.
fn narrow<T>(spec: &Value, key: &str, field: &str) -> Result<Option<T>, ModelError>
where
    T: TryFrom<u64> + Copy,
{
    let Some(v) = spec.get(key).and_then(|v| v.as_u64()) else {
        return Ok(None);
    };
    T::try_from(v)
        .map(Some)
        .map_err(|_| ModelError::OutOfRange {
            field: field.to_string(),
            value: v,
            ty: std::any::type_name::<T>(),
            max: match std::mem::size_of::<T>() {
                1 => u64::from(u8::MAX),
                2 => u64::from(u16::MAX),
                4 => u64::from(u32::MAX),
                _ => u64::MAX,
            },
        })
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

fn resources_of(device: &str, spec: &Value) -> Result<Option<Resources>, ModelError> {
    let Some(r) = spec.get("resources") else {
        return Ok(None);
    };
    let memory_key = if r.get("memory").is_some() {
        "memory"
    } else {
        "memory_gb"
    };
    Ok(Some(Resources {
        cpu: narrow(r, "cpu", &format!("{device}.resources.cpu"))?,
        memory_gb: narrow(r, memory_key, &format!("{device}.resources.{memory_key}"))?,
        gpu: r.get("gpu").and_then(|v| v.as_str()).map(str::to_owned),
    }))
}

/// The one part targeting `component`. A `stack`-layer part wins over its own members: a stack
/// exists precisely to be the single pullable unit, so carrying both is not ambiguous.
fn sole_part(
    ws: &Workspace,
    component: &str,
    backend: Option<Backend>,
) -> Result<Part, ModelError> {
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
    let kind = part_kind(component, chosen, backend)?;
    Ok(Part {
        reference: chosen.reference.clone(),
        kind,
        digest: chosen.digest.clone(),
        local: chosen.is_local(),
    })
}

/// What a part *is*, and whether the device can run it.
///
/// Three sources, in order: the baseline's explicit `kind:`, then the reference itself, then
/// the device's backend. The last is a fallback rather than the rule — deriving the kind from
/// the backend is what made a mismatch *unrepresentable*: a baseline pointing a `libvirt` arm at
/// a container image simply became a bundle, and the run failed at boot with nothing naming the
/// cause. Wherever the part says what it is, it is checked against the backend instead.
fn part_kind(
    component: &str,
    entry: &PartEntry,
    backend: Option<Backend>,
) -> Result<PartKind, ModelError> {
    let declared = match entry.kind.as_deref() {
        None => None,
        Some("container") => Some(PartKind::Container),
        Some("bundle") => Some(PartKind::Bundle),
        Some(other) => {
            return Err(ModelError::UnknownPartKind {
                component: component.to_string(),
                part: entry.reference.clone(),
                kind: other.to_string(),
            });
        }
    };
    let known = declared.or_else(|| kind_from_reference(&entry.reference));
    let Some(kind) = known else {
        // A bare path says nothing about itself, so the backend decides — the one case where
        // guessing is the only option, and the one the `kind:` field exists to remove.
        return Ok(match backend {
            Some(Backend::Container) | None => PartKind::Container,
            Some(_) => PartKind::Bundle,
        });
    };
    let runnable = matches!(
        (kind, backend),
        (_, None)
            | (PartKind::Container, Some(Backend::Container))
            | (
                PartKind::Bundle,
                Some(Backend::Libvirt | Backend::QemuMicrovm)
            )
    );
    if !runnable {
        return Err(ModelError::PartKindMismatch {
            component: component.to_string(),
            part: entry.reference.clone(),
            kind: match kind {
                PartKind::Container => "container",
                PartKind::Bundle => "bundle",
            },
            backend: match backend {
                Some(Backend::Container) => "container",
                Some(Backend::QemuMicrovm) => "qemu-microvm",
                Some(Backend::Libvirt) => "libvirt",
                None => "none",
            },
        });
    }
    Ok(kind)
}

/// What a reference says about itself — deliberately little.
///
/// Only the `local/…` namespace is conclusive: it names a host process or a locally-built
/// container image, never a guest bundle directory, and pointing a VM backend at one is exactly
/// the mistake the exp-001 migration hit (it planned as a VM with no bundle to boot).
///
/// A published vxparts ref is **not** used, though it looks like it should be: its class says
/// what kind of *node* the part loads onto (`zone` = virtual ECU), not how the part is
/// packaged, and a zone guest is legitimately a container under the netns backend. Reading
/// packaging out of the class would reject correct workspaces, which is worse than the gap.
/// That is what `kind:` on the baseline entry is for.
fn kind_from_reference(reference: &str) -> Option<PartKind> {
    reference
        .starts_with("local/")
        .then_some(PartKind::Container)
}

fn link_endpoint(doc: &crate::workspace::EndpointDoc) -> Endpoint {
    Endpoint {
        node: doc.node().to_string(),
        port: doc.port().map(str::to_owned),
        ip_cidr: doc.ip().map(str::to_owned),
    }
}

/// A channel endpoint names a *function*, and carries no address — see [`Endpoint::ip_cidr`].
fn channel_endpoint(raw: &str) -> Endpoint {
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

fn role_of(device: &str, raw: &str) -> Result<DeviceRole, ModelError> {
    Ok(match raw {
        "zone-controller" => DeviceRole::ZoneController,
        "hpc" => DeviceRole::Hpc,
        "gateway" => DeviceRole::Gateway,
        "ecu" => DeviceRole::Ecu,
        "sensor" => DeviceRole::Sensor,
        "actuator" => DeviceRole::Actuator,
        "switch" => DeviceRole::Switch,
        other => {
            return Err(ModelError::UnknownRole {
                device: device.to_string(),
                role: other.to_string(),
            });
        }
    })
}

fn asil_of(raw: &str) -> Option<Asil> {
    match raw.to_ascii_uppercase().as_str() {
        "QM" => Some(Asil::Qm),
        "A" => Some(Asil::A),
        "B" => Some(Asil::B),
        "C" => Some(Asil::C),
        "D" => Some(Asil::D),
        _ => None,
    }
}

/// §6.0 → entity. Nothing is synthesized: the schema declares zones, devices and functions as
/// separate things, so this reads them. The earlier builder had to invent a function per device
/// because §6.0 could not say what a function was — that was the gap this schema closed.
fn build_entity(ws: &Workspace, doc: &VehicleDoc) -> Result<Entity, ModelError> {
    let zones = doc
        .zones
        .iter()
        .map(|(id, z)| {
            (
                id.clone(),
                Zone {
                    id: id.clone(),
                    name: z.name.clone(),
                    reference: z.reference.clone(),
                },
            )
        })
        .collect();

    let mut devices: BTreeMap<String, Device> = BTreeMap::new();
    for (id, d) in &doc.devices {
        let spec = serde_yaml_ng::to_value(d).unwrap_or(Value::Null);
        devices.insert(
            id.clone(),
            Device {
                id: id.clone(),
                role: role_of(id, &d.role)?,
                zone: d.zone.clone(),
                resources: if d.resources.is_some() {
                    resources_of(id, &spec)?
                } else {
                    None
                },
                backend: backend_of(id, &spec)?,
                signals: match (&d.vss, &d.dbc) {
                    (None, None) => Vec::new(),
                    (vss, dbc) => vec![SignalBinding {
                        id: id.clone(),
                        vss: vss.clone(),
                        dbc: dbc.clone(),
                    }],
                },
                pinned_cpus: d
                    .realtime
                    .as_ref()
                    .and_then(|r| r.get("pinned_cpus"))
                    .and_then(|v| v.as_sequence())
                    .map(|q| {
                        q.iter()
                            .filter_map(|v| v.as_u64())
                            .filter_map(|v| u32::try_from(v).ok())
                            .collect()
                    })
                    .unwrap_or_default(),
            },
        );
    }

    let mut functions: BTreeMap<String, Function> = BTreeMap::new();
    for (id, f) in &doc.functions {
        let backend = devices.get(&f.deployed_on).and_then(|d| d.backend);
        functions.insert(
            id.clone(),
            Function {
                id: id.clone(),
                deployed_on: f.deployed_on.clone(),
                // "No part at all" is tolerated here and reported by the workspace validator
                // against the file the author wrote (`ComponentWithoutPart`). Any *other*
                // failure is a real defect in the part they did declare, and swallowing it
                // turned a kind mismatch into a function that silently ran nothing.
                part: match sole_part(ws, id, backend) {
                    Ok(part) => Some(part),
                    Err(ModelError::NoPart { .. }) => None,
                    Err(e) => return Err(e),
                },
                asil: f.asil.as_deref().and_then(asil_of),
            },
        );
    }

    Ok(Entity {
        id: doc.id.clone(),
        class: match doc.class.as_deref() {
            Some("air") => EntityClass::Air,
            Some("marine") => EntityClass::Marine,
            Some("rail") => EntityClass::Rail,
            Some("road") | None => EntityClass::Road,
            Some(other) => EntityClass::Other(other.to_string()),
        },
        subtype: doc.subtype.clone(),
        zones,
        devices,
        functions,
        links: doc
            .links
            .iter()
            .map(|l| Link {
                id: l.id.clone(),
                endpoints: l.endpoints.iter().map(link_endpoint).collect(),
                kind: link_kind(l.slice.as_ref()),
                slice: l.slice.clone(),
                harness: None,
            })
            .collect(),
        channels: doc
            .channels
            .iter()
            .map(|c| Channel {
                id: c.id.clone(),
                from: channel_endpoint(&c.from),
                to: c.to.iter().map(|e| channel_endpoint(e)).collect(),
                protocol: Protocol::Udp,
                qos: None,
                carried_by: c.carried_by.clone(),
            })
            .collect(),
    })
}

fn slices_of(raw: &Value) -> Result<Vec<Slice>, ModelError> {
    let Some(list) = raw
        .get("network")
        .and_then(|n| n.get("slices"))
        .and_then(|s| s.as_sequence())
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for s in list {
        let Some(id) = s.get("id").and_then(|v| v.as_str()).map(str::to_owned) else {
            continue;
        };
        let mode = match s.get("mode").and_then(|m| m.as_str()) {
            Some("s-nssai") => SliceMode::SNssai,
            _ => SliceMode::Vlan,
        };
        let qos = match s.get("qos") {
            Some(q) => Some(SliceQos {
                min_rate_mbps: narrow(
                    q,
                    "min_rate_mbps",
                    &format!("slice {id}.qos.min_rate_mbps"),
                )?,
                max_rate_mbps: narrow(
                    q,
                    "max_rate_mbps",
                    &format!("slice {id}.qos.max_rate_mbps"),
                )?,
                priority: narrow(q, "priority", &format!("slice {id}.qos.priority"))?,
            }),
            None => None,
        };
        let s_nssai = match s.get("s_nssai") {
            Some(n) => {
                narrow::<u8>(n, "sst", &format!("slice {id}.s_nssai.sst"))?.map(|sst| SNssai {
                    sst,
                    sd: n.get("sd").and_then(|v| v.as_str()).map(str::to_owned),
                })
            }
            None => None,
        };
        out.push(Slice {
            vlan: narrow(s, "vlan", &format!("slice {id}.vlan"))?,
            id,
            mode,
            s_nssai,
            qos,
        });
    }
    Ok(out)
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

/// Everything that can go wrong between "some YAML" and "a checked model".
///
/// One type because callers act the same way on all three — report and stop — and because the
/// daemon, the CLI and every test want the same one-call entry point rather than a three-step
/// dance each reimplements slightly differently.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Workspace(#[from] crate::workspace::WorkspaceError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(
        "model invariants violated:\n{}",
        .0.iter().map(|e| format!("  - {e}")).collect::<Vec<_>>().join("\n")
    )]
    Invariants(Vec<InvariantError>),
}

/// Workspace directory → checked model. The single entry point for provisioning.
pub fn load_dir(root: &std::path::Path) -> Result<Experiment, LoadError> {
    checked(build(&Workspace::load_validated(root)?)?)
}

/// Workspace YAML text → checked model, for callers with no directory to point at: an inline
/// manifest over vxAPI, and every test fixture in this crate.
pub fn load_yaml(text: &str) -> Result<Experiment, LoadError> {
    let ws = Workspace::from_yaml(text)?;
    let errors = ws.validate();
    if !errors.is_empty() {
        return Err(LoadError::Workspace(
            crate::workspace::WorkspaceError::Invalid(errors),
        ));
    }
    checked(build(&ws)?)
}

fn checked(exp: Experiment) -> Result<Experiment, LoadError> {
    let violations = exp.check();
    if violations.is_empty() {
        Ok(exp)
    } else {
        Err(LoadError::Invariants(violations))
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
                        pps: narrow(v, "pps", "traffic.pps").ok()?,
                        bandwidth_mbps: narrow(v, "bandwidth_mbps", "traffic.bandwidth_mbps")
                            .ok()?,
                        payload_bytes: narrow(v, "payload_bytes", "traffic.payload_bytes").ok()?,
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
        slices: slices_of(&raw)?,
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
        let f = e.functions.get("adas").expect("declared function");
        assert_eq!(f.deployed_on, "zonefl");
        assert!(f.part.as_ref().unwrap().reference.contains("probe-bundle"));
        assert_eq!(e.host_of("adas").unwrap().id, "zonefl");
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
    fn h1_fixture(name: &str) -> (std::path::PathBuf, Workspace) {
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
zones:
  front-left: {}
  rear-left: {}
devices:
  node-a: { role: zone-controller, zone: front-left, backend: container }
  node-b: { role: zone-controller, zone: rear-left, backend: container }
functions:
  fn-node-a: { deployed_on: node-a }
  fn-node-b: { deployed_on: node-b }
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
  - { ref: "local/vxprobe:host-process", target_ids: fn-node-a }
  - { ref: "local/vxtraffic:host-process", target_ids: fn-node-b }
"#,
        )
        .unwrap();

        let ws = Workspace::load_validated(&tmp).unwrap();
        (tmp, ws)
    }

    /// Golden assertion on the plans an H1-shaped workspace produces.
    ///
    /// This replaced a comparison against the §6.1 planner, which no longer exists — there is
    /// one planning path now. The values below were taken from that comparison while both paths
    /// were live and verified identical across all five frozen manifests, so they pin the
    /// behaviour rather than merely restating the implementation.
    #[test]
    fn an_h1_workspace_plans_the_expected_topology() {
        use crate::gateway::GatewayPlan;
        use crate::traffic::TrafficPlan;
        use crate::zones::plan::plan_nodes_from_model;

        let (tmp, ws) = h1_fixture("golden");
        let model = build(&ws).unwrap();
        assert!(model.check().is_empty(), "{:?}", model.check());

        let gw = GatewayPlan::from_model(&model).unwrap();
        assert_eq!(gw.root_rate_mbps, 500, "shared uplink bottleneck");
        let eps: Vec<(&str, &str, u16)> = gw
            .endpoints
            .iter()
            .map(|e| (e.name.as_str(), e.peer.as_str(), e.vlan))
            .collect();
        assert_eq!(
            eps,
            vec![
                ("vz-node-a", "vzp-node-a", 101),
                ("vz-node-b", "vzp-node-b", 103)
            ]
        );

        let nodes = plan_nodes_from_model(&model, &gw, &tmp).unwrap();
        assert_eq!(
            nodes.iter().map(|n| n.node_id.as_str()).collect::<Vec<_>>(),
            vec!["node-a", "node-b"]
        );
        assert_eq!(nodes[0].runtime_name(), "vx-node-a");

        let traffic = TrafficPlan::from_model(&model, &gw).unwrap();
        assert_eq!(traffic.profiles.len(), 1);
        assert_eq!(traffic.profiles[0].id, "load");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The whole point of the object endpoint form: a pinned address has to survive from the
    /// vehicle all the way to the zone backend that configures it. exp-002's three arms share one
    /// VLAN and are told apart by address alone, so an address dropped anywhere on this path is
    /// three arms answering at nothing.
    #[test]
    fn a_pinned_address_reaches_the_node_plan() {
        use crate::gateway::GatewayPlan;
        use crate::zones::plan::plan_nodes_from_model;

        let (tmp, _) = h1_fixture("addressed");
        let veh = std::fs::read_to_string(tmp.join("veh.yaml")).unwrap().replace(
            "  - { id: la, endpoints: [node-a:eth0], slice: adas }",
            "  - { id: la, endpoints: [{ node: node-a, port: eth0, ip: 10.101.0.11/24 }], slice: adas }",
        );
        std::fs::write(tmp.join("veh.yaml"), veh).unwrap();

        let ws = Workspace::load_validated(&tmp).expect("still validates");
        let model = build(&ws).unwrap();
        let gw = GatewayPlan::from_model(&model).unwrap();
        let nodes = plan_nodes_from_model(&model, &gw, &tmp).unwrap();
        let node_a = nodes.iter().find(|n| n.node_id == "node-a").unwrap();
        assert_eq!(node_a.ip_cidr.as_deref(), Some("10.101.0.11/24"));
        // …and an unaddressed attachment stays unaddressed, so a vehicle that pins nothing keeps
        // working across experiments with different subnets.
        let node_b = nodes.iter().find(|n| n.node_id == "node-b").unwrap();
        assert_eq!(node_b.ip_cidr, None);
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// H3 runs one function on three backends from three parts, so a baseline pointing a VM arm
    /// at a container image is the mistake most worth catching — and it used to be
    /// unrepresentable rather than rejected, because the kind was *derived* from the backend.
    #[test]
    fn a_part_the_backend_cannot_run_is_refused_with_both_halves_named() {
        // `local/…` names a host process, never a bootable bundle, so libvirt cannot run it.
        let ws = "
kind: experiment
id: e
vehicles: [veh]
baseline: bl
network: { slices: [{ id: adas, mode: vlan, vlan: 101 }] }
---
kind: vehicle
id: veh
devices: { vm1: { role: ecu, backend: libvirt } }
functions: { fn-vm: { deployed_on: vm1 } }
links: [{ id: l, endpoints: [vm1:eth0], slice: adas }]
---
kind: baseline
id: bl
parts: [{ ref: 'local/vxprobe:host-process', target_ids: fn-vm }]
";
        match load_yaml(ws) {
            Err(crate::model::LoadError::Model(ModelError::PartKindMismatch {
                kind,
                backend,
                ..
            })) => {
                assert_eq!(kind, "container");
                assert_eq!(backend, "libvirt");
            }
            other => panic!("expected PartKindMismatch, got {other:?}"),
        }

        // An explicit `kind:` is authoritative where the reference is silent — and is checked
        // the same way, so a path ref can be wrong out loud too.
        let bad_explicit = ws.replace(
            "parts: [{ ref: 'local/vxprobe:host-process', target_ids: fn-vm }]",
            "parts: [{ ref: ./image, kind: container, target_ids: fn-vm }]",
        );
        assert!(matches!(
            load_yaml(&bad_explicit),
            Err(crate::model::LoadError::Model(
                ModelError::PartKindMismatch { .. }
            ))
        ));

        // …and the same path ref declared a bundle is accepted, which is what makes the field
        // worth having rather than merely restrictive.
        let good_explicit = ws.replace(
            "parts: [{ ref: 'local/vxprobe:host-process', target_ids: fn-vm }]",
            "parts: [{ ref: ./bundle, kind: bundle, target_ids: fn-vm }]",
        );
        let m = load_yaml(&good_explicit).expect("a bundle on a VM backend is fine");
        assert_eq!(
            m.entities[0].functions["fn-vm"].part.as_ref().unwrap().kind,
            PartKind::Bundle
        );
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

    /// §6.1 gets range checking free: serde types the field `u16` and rejects anything larger.
    /// The model reads `as_u64()`, so narrowing had to be checked — it was not, and `vlan: 70000`
    /// silently became VLAN 4464, provisioning a different slice with nothing reported.
    #[test]
    fn an_out_of_range_vlan_is_rejected_rather_than_wrapped() {
        let (tmp, mut ws) = scaffolded("range");
        let net = ws.experiments_raw[0]
            .as_mapping_mut()
            .unwrap()
            .get_mut("network")
            .unwrap()
            .as_mapping_mut()
            .unwrap();
        net.get_mut("slices").unwrap().as_sequence_mut().unwrap()[0]
            .as_mapping_mut()
            .unwrap()
            .insert(
                serde_yaml_ng::Value::String("vlan".into()),
                serde_yaml_ng::Value::Number(70000.into()),
            );
        match build(&ws) {
            Err(ModelError::OutOfRange { value, .. }) => assert_eq!(value, 70000),
            other => panic!("expected OutOfRange, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A device the planner cannot realise is refused with the fix in the message rather than
    /// defaulted — the guess is exactly what produced a workspace that validated and then could
    /// not boot during the exp-001 migration.
    #[test]
    fn a_slice_attached_device_without_a_backend_is_refused() {
        use crate::gateway::GatewayPlan;
        // The scaffold declares a backend on every device now, so strip one to test the refusal.
        let (tmp, mut ws) = scaffolded("nobackend");
        ws.vehicles[0].devices.get_mut("zonefl").unwrap().backend = None;
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
        ws.vehicles[0].devices.get_mut("zonefl").unwrap().backend = Some("xen".into());
        match build(&ws) {
            Err(ModelError::UnknownBackend { backend, .. }) => assert_eq!(backend, "xen"),
            other => panic!("expected UnknownBackend, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
