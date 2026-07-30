//! Lowering: a §6.0 workspace → the §6.1 experiment manifest the daemon already runs.
//!
//! This is the bridge between the workspace model and today's execution path. It is a pure
//! transformation, not orchestration — the CLI stays a 1:1 vxAPI wrapper (hard rule,
//! `AGENTS.md`), and the daemon is unchanged.
//!
//! Two properties are deliberate:
//!
//! 1. **The output crosses the existing validated boundary.** Lowering builds YAML and hands
//!    it to [`ExperimentManifest::from_yaml`], so a lowering bug surfaces as a §6.1 validation
//!    error rather than a silently wrong manifest that provisions something unintended.
//! 2. **It proves the workspace model carries enough to run.** If a workspace cannot produce a
//!    valid §6.1 manifest, the model is missing something — which is a far more useful test of
//!    §6.0 than reading it.
//!
//! What it cannot carry is documented rather than hidden: see [`LowerError::LayeredStack`] and
//! the note on `channels` below.

use crate::manifest::ExperimentManifest;
use crate::workspace::{PartEntry, Workspace};
use serde_yaml_ng::{Mapping, Value};

#[derive(Debug, thiserror::Error)]
pub enum LowerError {
    #[error("workspace has no `kind: experiment` document to lower")]
    NoExperiment,
    #[error("experiment `{experiment}` names no vehicle — nothing to lower")]
    NoVehicle { experiment: String },
    #[error(
        "experiment `{experiment}` references vehicle `{vehicle}`, which is not in this workspace"
    )]
    UnknownVehicle { experiment: String, vehicle: String },
    #[error(
        "component `{component}` has {count} parts, but the §6.1 schema this lowers to carries \
         exactly one `image` per node. Publish them as a single `stack`-layer composite part \
         and reference that, or keep the component to one part until `experiment up` consumes \
         the workspace shape directly."
    )]
    LayeredStack { component: String, count: usize },
    #[error("component `{component}` has no part in baseline `{baseline}` — nothing to boot")]
    NoPart { component: String, baseline: String },
    #[error("zone component `{component}` has no `position`, which the §6.1 `role` field requires")]
    ZoneWithoutPosition { component: String },
    #[error(
        "traffic profile `{profile}` uses compute component `{component}` as its `{field}`. \
         §6.1 resolves traffic endpoints to zone nodes only — point this side at a zone \
         component, or at the symbolic `internet` reflector."
    )]
    ComputeTrafficEndpoint {
        profile: String,
        field: &'static str,
        component: String,
    },
    #[error("lowered manifest is not valid §6.1: {0}")]
    Invalid(#[from] crate::manifest::ManifestError),
    #[error("serialising the lowered manifest: {0}")]
    Serialise(#[from] serde_yaml_ng::Error),
}

/// Everything that can go wrong turning a workspace directory into a runnable manifest.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceLowerError {
    #[error(transparent)]
    Workspace(#[from] crate::workspace::WorkspaceError),
    #[error(transparent)]
    Lower(#[from] LowerError),
    #[error("serialising the lowered manifest: {0}")]
    Serialise(#[from] serde_yaml_ng::Error),
}

/// Load, validate and lower the workspace at `root`, returning §6.1 YAML.
///
/// The whole path from "a folder on disk" to "a manifest the provisioning layer runs", in one
/// call, so a caller never has to know the intermediate steps exist. `vxedged` uses this when a
/// `ManifestRef.path` names a directory — which is what keeps lowering out of every frontend.
pub fn lower_path_to_yaml(root: &std::path::Path) -> Result<String, WorkspaceLowerError> {
    let ws = Workspace::load_validated(root)?;
    let manifest = lower(&ws)?;
    Ok(serde_yaml_ng::to_string(&manifest)?)
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// `component:port` → the two halves.
fn split_endpoint(ep: &str) -> (&str, Option<&str>) {
    match ep.split_once(':') {
        Some((c, p)) => (c, Some(p)),
        None => (ep, None),
    }
}

/// A part reference's class segment, when it is a published vxparts ref.
/// `ghcr.io/vxedge/parts/<class>/…` → `<class>`.
fn part_class(reference: &str) -> Option<&str> {
    let (_, after) = reference.split_once("/parts/")?;
    after.split('/').next()
}

/// Which `image.kind` a component's part becomes. Derived from the published ref's class where
/// there is one, and otherwise from the component group — a local path carries no taxonomy, and
/// guessing from the filename would be worse than using the group we already know.
fn image_kind(group: &str, reference: &str) -> &'static str {
    match part_class(reference) {
        Some("zone") => "vm",
        Some(_) => "container",
        None => {
            if group == "zones" {
                "vm"
            } else {
                "container"
            }
        }
    }
}

/// Parts targeting one component, in baseline order.
fn parts_for<'a>(ws: &'a Workspace, component: &str) -> Vec<(&'a str, &'a PartEntry)> {
    let mut out = Vec::new();
    for b in &ws.baselines {
        for p in &b.parts {
            if let Some(t) = &p.target_ids
                && t.as_slice().contains(&component)
            {
                out.push((b.id.as_str(), p));
            }
        }
    }
    out
}

/// The single part a component boots, or an error explaining why there isn't one.
///
/// A `stack`-layer part wins when present: a stack exists precisely to be the one pullable
/// unit over its constituents, so a component carrying a stack plus its own members is not
/// ambiguous — the stack is the answer.
fn sole_part<'a>(ws: &'a Workspace, component: &str) -> Result<&'a PartEntry, LowerError> {
    let candidates = parts_for(ws, component);
    if candidates.is_empty() {
        return Err(LowerError::NoPart {
            component: component.to_string(),
            baseline: ws
                .baselines
                .first()
                .map(|b| b.id.clone())
                .unwrap_or_else(|| "<none>".into()),
        });
    }
    let stacks: Vec<_> = candidates
        .iter()
        .filter(|(_, p)| p.reference.contains("/stack/"))
        .collect();
    match (stacks.len(), candidates.len()) {
        (1, _) => Ok(stacks[0].1),
        (0, 1) => Ok(candidates[0].1),
        (_, n) => Err(LowerError::LayeredStack {
            component: component.to_string(),
            count: n,
        }),
    }
}

/// `slice id → vlan`, for the `mode: vlan` slices the experiment defines. §6.0 keeps the vlan on
/// the slice alone — a link names the slice and nothing else — but §6.1 repeats it on every node
/// iface, and `GatewayPlan` builds an endpoint only where that `vlan` is present. So lowering has
/// to resolve it; without this a lowered workspace provisions zones with no VLAN attachment and
/// no shaped traffic at all.
fn slice_vlans(network: Option<&Value>) -> Vec<(String, Value)> {
    let Some(slices) = network
        .and_then(|n| n.get("slices"))
        .and_then(|s| s.as_sequence())
    else {
        return Vec::new();
    };
    slices
        .iter()
        .filter_map(|sl| {
            let id = sl.get("id")?.as_str()?.to_string();
            Some((id, sl.get("vlan")?.clone()))
        })
        .collect()
}

/// Interfaces a component owns, derived from the links that touch it. `slice` rides along so
/// the lowered node binds to the same slice the vehicle declared, and `vlan` with it.
fn ifaces_for(
    ws: &Workspace,
    vehicle_id: &str,
    component: &str,
    vlans: &[(String, Value)],
) -> Vec<Value> {
    let mut out = Vec::new();
    for v in ws.vehicles.iter().filter(|v| v.id == vehicle_id) {
        for l in &v.links {
            for ep in &l.endpoints {
                let (c, port) = split_endpoint(ep);
                if c == component {
                    let mut m = Mapping::new();
                    m.insert(s("iface"), s(port.unwrap_or("eth0")));
                    if let Some(slice) = &l.slice {
                        if let Some((_, vlan)) = vlans.iter().find(|(id, _)| id == slice) {
                            m.insert(s("vlan"), vlan.clone());
                        }
                        m.insert(s("slice"), s(slice));
                    }
                    out.push(Value::Mapping(m));
                }
            }
        }
    }
    out
}

/// How a component is *realised* — `backend` (the H3 variable itself) and `image.kind`.
///
/// Found by migrating exp-001: neither was expressible in §6.0, so every zone lowered to
/// `kind: vm` with no backend, which `resolved_backend` turns into `libvirt`, which then fails
/// at zone-plan time with `MissingBundle` because a host-process ref is not a bundle. exp-001
/// would have validated cleanly and refused to run.
///
/// These belong on the component rather than on the part because they describe how this node
/// is realised, not what software it carries — which is precisely why H3 can vary the backend
/// across arms while holding the software identical.
fn realisation(spec: &Value) -> (Option<&str>, Option<&str>) {
    (
        spec.get("backend").and_then(|b| b.as_str()),
        spec.get("image_kind").and_then(|k| k.as_str()),
    )
}

fn image_value(part: &PartEntry, group: &str) -> Value {
    let mut m = Mapping::new();
    // Digest-pinned refs lower to the pinned form: the point of pinning is that the thing
    // actually provisioned is the thing that was pinned.
    let url = match &part.digest {
        Some(d) if !part.is_local() => format!("{}@{}", part.reference, d),
        _ => part.reference.clone(),
    };
    m.insert(s("url"), s(&url));
    m.insert(s("kind"), s(image_kind(group, &part.reference)));
    Value::Mapping(m)
}

/// `image_value` with the component's declared `image_kind`, or a backend-implied one, winning
/// over the group heuristic. A `container` backend cannot boot a `vm` image, so leaving the
/// guess in place would produce a manifest that contradicts itself.
fn image_value_for(
    part: &PartEntry,
    group: &str,
    backend: Option<&str>,
    image_kind_override: Option<&str>,
) -> Value {
    let mut v = image_value(part, group);
    let explicit = image_kind_override.or(match backend {
        Some("container") => Some("container"),
        _ => None,
    });
    if let (Some(kind), Some(m)) = (explicit, v.as_mapping_mut()) {
        m.insert(s("kind"), s(kind));
    }
    v
}

/// Lower `ws` to a §6.1 experiment manifest.
///
/// **Not carried:** `channels`. The §6.1 schema has no interaction layer, so a workspace's
/// messaging topology is modelled and validated but does not survive lowering. That is a real
/// limitation of the older schema rather than of the workspace model, and it is why wiring
/// `experiment up` to the workspace shape directly is the follow-up rather than this.
pub fn lower(ws: &Workspace) -> Result<ExperimentManifest, LowerError> {
    let exp = ws.experiments.first().ok_or(LowerError::NoExperiment)?;
    let raw = ws.experiments_raw.first().cloned().unwrap_or(Value::Null);

    let vehicle_id = exp.vehicles.first().ok_or_else(|| LowerError::NoVehicle {
        experiment: exp.id.clone(),
    })?;
    let vehicle = ws
        .vehicles
        .iter()
        .find(|v| &v.id == vehicle_id)
        .ok_or_else(|| LowerError::UnknownVehicle {
            experiment: exp.id.clone(),
            vehicle: vehicle_id.clone(),
        })?;

    let mut root = Mapping::new();

    // experiment block
    let mut e = Mapping::new();
    e.insert(s("id"), s(&exp.id));
    e.insert(s("name"), s(&exp.id));
    if let Some(m) = raw.get("method")
        && let Some(d) = m.get("duration_s")
    {
        e.insert(s("duration_s"), d.clone());
    }
    if let Some(r) = raw.get("results")
        && let Some(dir) = r.get("artifact_dir")
    {
        e.insert(s("results_prefix"), dir.clone());
    }
    root.insert(s("experiment"), Value::Mapping(e));

    // §6.1-shaped blocks pass through untouched (§6.0.3).
    for key in ["network", "services", "telemetry", "results"] {
        if let Some(v) = raw.get(key) {
            root.insert(s(key), v.clone());
        }
    }

    // …except traffic endpoints, which §6.1 spells `<zone-vxid>:<node-vxid>`. That zone vxid is
    // synthesized *here* (see `zone_value`), so a workspace author cannot name it and must not
    // have to: they write the component id and lowering qualifies it.
    if let Some(v) = raw.get("traffic_profiles") {
        root.insert(s("traffic_profiles"), lower_traffic(vehicle, v)?);
    }

    // zones + gateway → §6.1 `zones` (the gateway lands as ZoneRole::Gateway, which is exactly
    // the conflation §6.0 splits — visible here because lowering has to undo the split).
    let vlans = slice_vlans(raw.get("network"));
    let ctx = ZoneCtx {
        ws,
        vehicle,
        vlans: &vlans,
    };
    let mut zones = Vec::new();
    for (id, spec) in &vehicle.composition.zones {
        let position = spec
            .get("position")
            .and_then(|p| p.as_str())
            .ok_or_else(|| LowerError::ZoneWithoutPosition {
                component: id.clone(),
            })?;
        zones.push(zone_value(&ctx, id, position, "ecu", "zones", spec)?);
    }
    for (id, spec) in &vehicle.composition.gateway {
        zones.push(zone_value(&ctx, id, "gateway", "gateway", "gateway", spec)?);
    }
    if !zones.is_empty() {
        root.insert(s("zones"), Value::Sequence(zones));
    }

    // compute
    let mut compute = Vec::new();
    for (id, spec) in &vehicle.composition.compute {
        let part = sole_part(ws, id)?;
        let mut m = Mapping::new();
        let (backend, kind) = realisation(spec);
        m.insert(s("id"), s(id));
        m.insert(s("image"), image_value_for(part, "compute", backend, kind));
        if let Some(r) = spec.get("resources") {
            m.insert(s("resources"), r.clone());
        }
        compute.push(Value::Mapping(m));
    }
    if !compute.is_empty() {
        root.insert(s("compute"), Value::Sequence(compute));
    }

    // links: §6.0 endpoints[2] → §6.1 from/to
    let mut links = Vec::new();
    for l in &vehicle.links {
        if l.endpoints.len() != 2 {
            continue;
        }
        let mut m = Mapping::new();
        m.insert(s("from"), s(&l.endpoints[0]));
        m.insert(s("to"), s(&l.endpoints[1]));
        m.insert(
            s("type"),
            s(if l.slice.is_some() {
                "vlan_trunk"
            } else {
                "direct"
            }),
        );
        links.push(Value::Mapping(m));
    }
    if !links.is_empty() {
        root.insert(s("links"), Value::Sequence(links));
    }

    let yaml = serde_yaml_ng::to_string(&Value::Mapping(root))?;
    Ok(ExperimentManifest::from_yaml(&yaml)?)
}

/// Qualify the `source`/`target` of every traffic profile.
///
/// A component id that lowers to a zone becomes `vxzone-<id>:<id>`. A compute component is
/// refused rather than silently downgraded to a symbolic endpoint — §6.1 resolves traffic
/// endpoints against zone nodes only, so a compute target would quietly stop being measured.
/// Anything else passes through: that is how the symbolic `internet` reflector survives.
fn lower_traffic(
    vehicle: &crate::workspace::VehicleDoc,
    profiles: &Value,
) -> Result<Value, LowerError> {
    let Some(map) = profiles.as_mapping() else {
        return Ok(profiles.clone());
    };
    let mut out = Mapping::new();
    for (name, profile) in map {
        let profile_id = name.as_str().unwrap_or("<unnamed>");
        let Some(fields) = profile.as_mapping() else {
            out.insert(name.clone(), profile.clone());
            continue;
        };
        let mut lowered = fields.clone();
        for field in ["source", "target"] {
            let Some(component) = fields.get(field).and_then(|v| v.as_str()) else {
                continue;
            };
            if vehicle.composition.compute.contains_key(component) {
                return Err(LowerError::ComputeTrafficEndpoint {
                    profile: profile_id.to_string(),
                    field,
                    component: component.to_string(),
                });
            }
            if vehicle.composition.zones.contains_key(component)
                || vehicle.composition.gateway.contains_key(component)
            {
                lowered.insert(s(field), s(&format!("vxzone-{component}:{component}")));
            }
        }
        out.insert(name.clone(), Value::Mapping(lowered));
    }
    Ok(Value::Mapping(out))
}

/// The parts of a lowering that are the same for every component: the workspace, the vehicle
/// being lowered, and the slice→vlan table. Grouped so `zone_value` takes what varies.
struct ZoneCtx<'a> {
    ws: &'a Workspace,
    vehicle: &'a crate::workspace::VehicleDoc,
    vlans: &'a [(String, Value)],
}

fn zone_value(
    ctx: &ZoneCtx<'_>,
    id: &str,
    role: &str,
    node_type: &str,
    group: &str,
    spec: &Value,
) -> Result<Value, LowerError> {
    let (ws, vehicle, vlans) = (ctx.ws, ctx.vehicle, ctx.vlans);
    let part = sole_part(ws, id)?;
    let (backend, kind) = realisation(spec);
    let mut node = Mapping::new();
    node.insert(s("id"), s(id));
    node.insert(s("type"), s(node_type));
    node.insert(s("image"), image_value_for(part, group, backend, kind));
    if let Some(b) = backend {
        node.insert(s("backend"), s(b));
    }
    let ifaces = ifaces_for(ws, &vehicle.id, id, vlans);
    if !ifaces.is_empty() {
        node.insert(s("network"), Value::Sequence(ifaces));
    }

    // §6.1 nests nodes inside a zone and requires both ids to be distinct, while a §6.0
    // component is a single thing. The component id stays on the *node*, because that is what
    // links and part targets refer to; the zone wrapper gets a synthesized id.
    let mut z = Mapping::new();
    z.insert(s("id"), s(&format!("vxzone-{id}")));
    z.insert(s("role"), s(role));
    z.insert(s("nodes"), Value::Sequence(vec![Value::Mapping(node)]));
    Ok(Value::Mapping(z))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::{Template, WorkspacePlan};

    fn scaffolded(template: Template, name: &str) -> (std::path::PathBuf, Workspace) {
        let tmp = std::env::temp_dir().join(format!("vxlower-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let root = WorkspacePlan::plan("demo", template)
            .unwrap()
            .write(&tmp)
            .unwrap();
        let ws = Workspace::load(&root).unwrap();
        (tmp, ws)
    }

    /// The test that makes §6.0 more than a proposal: a scaffolded workspace must lower to a
    /// manifest the *existing* validator accepts. If it cannot, the workspace model does not
    /// carry enough to run.
    #[test]
    fn every_template_lowers_to_a_valid_manifest() {
        for template in [Template::Minimal, Template::Slicing] {
            let (tmp, ws) = scaffolded(template, template.as_str());
            let m =
                lower(&ws).unwrap_or_else(|e| panic!("{} does not lower: {e}", template.as_str()));
            assert_eq!(m.experiment.id, "demo");
            assert!(!m.zones.is_empty(), "zones lost in lowering");
            assert!(!m.compute.is_empty(), "compute lost in lowering");
            std::fs::remove_dir_all(&tmp).ok();
        }
    }

    #[test]
    fn slices_and_ifaces_survive_lowering() {
        let (tmp, ws) = scaffolded(Template::Slicing, "slices");
        let m = lower(&ws).unwrap();
        let slices: Vec<_> = m
            .network
            .as_ref()
            .map(|n| n.slices.iter().map(|s| s.id.clone()).collect())
            .unwrap_or_default();
        assert!(slices.contains(&"adas".to_string()));
        assert!(slices.contains(&"infotainment".to_string()));

        // A node bound to a slice in the vehicle must still be bound after lowering, or the
        // provisioned topology would silently differ from the modelled one.
        let bound = m
            .zones
            .iter()
            .flat_map(|z| z.nodes.iter())
            .flat_map(|n| n.network.iter())
            .filter_map(|i| i.slice.clone())
            .collect::<Vec<_>>();
        assert!(bound.contains(&"adas".to_string()), "{bound:?}");

        // …and the slice's vlan must be resolved onto the iface. §6.0 keeps it on the slice
        // only; §6.1 needs it on the node, and `GatewayPlan` silently plans no endpoint without
        // it — so "slice survived" is not enough to know the topology is provisionable.
        assert!(
            m.zones
                .iter()
                .flat_map(|z| z.nodes.iter())
                .flat_map(|n| n.network.iter())
                .any(|i| i.slice.as_deref() == Some("adas") && i.vlan == Some(101)),
            "vlan not resolved from the slice onto the iface"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn the_gateway_lowers_into_the_conflated_zone_role() {
        // §6.1 has no gateway block — it is a ZoneRole. Lowering has to undo §6.0's split,
        // which is the clearest demonstration of why the split exists.
        let (tmp, ws) = scaffolded(Template::Minimal, "gw");
        let m = lower(&ws).unwrap();
        assert!(
            m.zones
                .iter()
                .any(|z| z.role == crate::manifest::ZoneRole::Gateway),
            "gateway did not survive lowering"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_digest_pinned_part_lowers_to_the_pinned_url() {
        let (tmp, ws) = scaffolded(Template::Minimal, "pin");
        let m = lower(&ws).unwrap();
        let urls: Vec<_> = m
            .zones
            .iter()
            .flat_map(|z| z.nodes.iter())
            .map(|n| n.image.url.clone())
            .collect();
        assert!(
            urls.iter().any(|u| u.contains("@sha256:")),
            "pin lost in lowering: {urls:?}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Lowering to something `ExperimentManifest::from_yaml` accepts is not enough: §6.1's
    /// composition validator never looks at traffic endpoints, so a lowered manifest can be
    /// "valid" and still resolve to no measurable traffic. Planning it is the real assertion.
    #[test]
    fn every_template_lowers_to_a_plannable_gateway_and_traffic_plan() {
        use crate::gateway::GatewayPlan;
        use crate::traffic::TrafficPlan;
        for template in [Template::Minimal, Template::Slicing] {
            let (tmp, ws) = scaffolded(template, &format!("plan-{}", template.as_str()));
            let m = lower(&ws).unwrap();
            let gw = GatewayPlan::from_manifest(&m)
                .unwrap_or_else(|e| panic!("{} does not gateway-plan: {e}", template.as_str()));
            let tp = TrafficPlan::from_manifest(&m, &gw)
                .unwrap_or_else(|e| panic!("{} does not traffic-plan: {e}", template.as_str()));
            assert!(
                !tp.profiles.is_empty(),
                "{} planned no traffic",
                template.as_str()
            );
            std::fs::remove_dir_all(&tmp).ok();
        }
    }

    /// A compute component as a traffic endpoint lowers to nothing §6.1 can resolve. Refuse it
    /// with the fix in the message rather than emitting a profile that measures nothing.
    #[test]
    fn a_compute_traffic_endpoint_is_refused() {
        let (tmp, ws) = scaffolded(Template::Minimal, "computeep");
        let mut ws = ws;
        let raw = ws.experiments_raw[0].as_mapping_mut().unwrap();
        let profiles = raw
            .get_mut("traffic_profiles")
            .unwrap()
            .as_mapping_mut()
            .unwrap();
        let probe = profiles
            .get_mut(s("probe"))
            .unwrap()
            .as_mapping_mut()
            .unwrap();
        probe.insert(s("target"), s("hpc1"));
        match lower(&ws) {
            Err(LowerError::ComputeTrafficEndpoint { component, .. }) => {
                assert_eq!(component, "hpc1")
            }
            other => panic!("expected ComputeTrafficEndpoint, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The exact call `vxedged` makes when a `ManifestRef.path` names a directory. Worth its own
    /// test because it is the whole daemon-side path from "a folder" to "a runnable manifest",
    /// and nothing else exercises the three steps as one unit.
    #[test]
    fn a_workspace_directory_lowers_straight_to_yaml() {
        let (tmp, _) = scaffolded(Template::Slicing, "topath");
        let root = tmp.join("demo");
        let yaml = lower_path_to_yaml(&root).expect("workspace lowers to yaml");
        let manifest = ExperimentManifest::from_yaml(&yaml).expect("and that yaml is valid §6.1");
        assert_eq!(manifest.experiment.id, "demo");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The migration invariant: a workspace that replaces a frozen §6.1 manifest must produce
    /// the **same provisioning plan**, not merely a valid one. Endpoint names are the sharp
    /// edge — `run-exp-001.sh` addresses `vzp0`/`vzp1` directly, so a migration that renumbered
    /// them would keep validating, keep running, and quietly measure the wrong interface.
    ///
    /// Modelled on exp-001 (H1): two container-backed zone nodes on separate vlan slices.
    #[test]
    fn a_migrated_workspace_plans_identically_to_the_manifest_it_replaces() {
        use crate::gateway::GatewayPlan;

        let frozen = ExperimentManifest::from_yaml(
            r#"
experiment: { id: exp-x, name: x, duration_s: 60 }
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, priority: 1 } }
    - { id: infot, mode: vlan, vlan: 103, qos: { min_rate_mbps: 0, priority: 5 } }
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
"#,
        )
        .expect("frozen manifest parses");

        let tmp = std::env::temp_dir().join(format!("vxequiv-{}", std::process::id()));
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
method: { duration_s: 60 }
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, priority: 1 } }
    - { id: infot, mode: vlan, vlan: 103, qos: { min_rate_mbps: 0, priority: 5 } }
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

        let migrated = ExperimentManifest::from_yaml(&lower_path_to_yaml(&tmp).unwrap()).unwrap();
        let a = GatewayPlan::from_manifest(&frozen).unwrap();
        let b = GatewayPlan::from_manifest(&migrated).unwrap();

        let names = |p: &GatewayPlan| {
            p.endpoints
                .iter()
                .map(|e| {
                    (
                        e.name.clone(),
                        e.peer.clone(),
                        e.node.clone(),
                        e.vlan,
                        e.slice.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&a), names(&b), "endpoint identity drifted");
        assert_eq!(a.summary(), b.summary(), "gateway plan drifted");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn a_layered_component_is_refused_with_an_explanation() {
        // §6.1 carries one image per node. Rather than silently pick one, say so — this is
        // the constraint that motivates `parts:` in the first place.
        let (tmp, mut ws) = scaffolded(Template::Minimal, "layered");
        let extra = ws.baselines[0].parts[0].clone();
        ws.baselines[0].parts.push(extra);
        match lower(&ws) {
            Err(LowerError::LayeredStack { count, .. }) => assert_eq!(count, 2),
            other => panic!("expected LayeredStack, got {other:?}"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
