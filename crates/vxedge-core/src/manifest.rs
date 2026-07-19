//! Experiment-manifest model, mirroring `docs/ARCHITECTURE.md` §6.1 exactly.
//! The CLI/daemon have no other validation source: unknown fields are errors.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest parse error: {0}")]
    Parse(#[from] serde_yaml_ng::Error),
    #[error("manifest validation failed:\n{}", .0.iter().map(|e| format!("  - {e}")).collect::<Vec<_>>().join("\n"))]
    Invalid(Vec<ValidationError>),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("duplicate vxid `{0}`")]
    DuplicateVxid(String),
    #[error("node `{node}` references unknown slice `{slice}`")]
    UnknownSlice { node: String, slice: String },
    #[error("slice `{0}` has mode `vlan` but no `vlan` id")]
    VlanSliceWithoutVlan(String),
    #[error("slice `{0}` has mode `s-nssai` but no `s_nssai` block")]
    SnssaiSliceWithoutSnssai(String),
    #[error("service `{svc}` referenced by `{referrer}` is not declared under `services`")]
    UnknownService { referrer: String, svc: String },
    #[error(
        "node `{0}` declares `slice_policy` but execution doesn't implement URSP-style \
         routing yet (schema-only, sized 2026-07-17 in vxedge/TASKS.md) — remove it or bind \
         the node to a single slice via `network[].slice` instead"
    )]
    SlicePolicyNotImplemented(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentManifest {
    pub experiment: Experiment,
    #[serde(default)]
    pub network: Option<Network>,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceDef>,
    #[serde(default)]
    pub compute: Vec<ComputeNode>,
    #[serde(default)]
    pub zones: Vec<Zone>,
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub traffic_profiles: BTreeMap<String, TrafficProfile>,
    #[serde(default)]
    pub telemetry: Option<Telemetry>,
    #[serde(default)]
    pub results: Option<Results>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Experiment {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub duration_s: Option<u64>,
    #[serde(default)]
    pub results_prefix: Option<String>,
    /// Optional path to a vehicle manifest this experiment composes (§6.2).
    #[serde(default)]
    pub vehicle: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    #[serde(default)]
    pub topology: Option<Topology>,
    #[serde(default)]
    pub defaults: Option<NetworkDefaults>,
    #[serde(default)]
    pub slices: Vec<Slice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Topology {
    Mesh,
    Star,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDefaults {
    #[serde(default)]
    pub bandwidth: Option<u64>,
    #[serde(default)]
    pub latency: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slice {
    pub id: String,
    pub mode: SliceMode,
    #[serde(default)]
    pub vlan: Option<u16>,
    #[serde(default)]
    pub s_nssai: Option<Snssai>,
    #[serde(default)]
    pub qos: Option<SliceQos>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SliceMode {
    #[serde(rename = "vlan")]
    Vlan,
    #[serde(rename = "s-nssai")]
    Snssai,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snssai {
    pub sst: u8,
    #[serde(default)]
    pub sd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SliceQos {
    #[serde(default)]
    pub min_rate_mbps: Option<u64>,
    #[serde(default)]
    pub max_rate_mbps: Option<u64>,
    #[serde(default)]
    pub priority: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDef {
    pub id: String,
    #[serde(rename = "type")]
    pub service_type: ServiceType,
    pub image: Image,
    #[serde(default)]
    pub config: Option<serde_yaml_ng::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceType {
    Ota,
    Telemetry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub url: String,
    pub kind: ImageKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageKind {
    Vm,
    Container,
    Rtimage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputeNode {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub image: Image,
    #[serde(default)]
    pub resources: Option<Resources>,
    #[serde(default)]
    pub services: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(default)]
    pub cpu: Option<u32>,
    #[serde(default)]
    pub memory: Option<u32>,
    #[serde(default)]
    pub gpu: Option<GpuMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GpuMode {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "passthrough")]
    Passthrough,
    #[serde(rename = "mig-slice")]
    MigSlice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Zone {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub role: ZoneRole,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub services: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ZoneRole {
    #[serde(rename = "front-left")]
    FrontLeft,
    #[serde(rename = "front-right")]
    FrontRight,
    #[serde(rename = "rear-left")]
    RearLeft,
    #[serde(rename = "rear-right")]
    RearRight,
    #[serde(rename = "central-hpc")]
    CentralHpc,
    #[serde(rename = "gateway")]
    Gateway,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    #[serde(default)]
    pub function: Option<String>,
    pub image: Image,
    /// Zone backend (§9): libvirt is the default for vm/rtimage kinds.
    #[serde(default)]
    pub backend: Option<Backend>,
    #[serde(default)]
    pub realtime: Option<Realtime>,
    #[serde(default)]
    pub network: Vec<NodeIface>,
    /// URSP-style UE-side slice selection (§6.1, phase 2).
    #[serde(default)]
    pub slice_policy: Vec<SlicePolicyRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeType {
    Ecu,
    Sensor,
    Gateway,
    Infotainment,
    TelemetryClient,
    Rsu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    #[serde(rename = "libvirt")]
    Libvirt,
    #[serde(rename = "qemu-microvm")]
    QemuMicrovm,
    #[serde(rename = "container")]
    Container,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Realtime {
    #[serde(default)]
    pub pinned_cpus: Vec<u32>,
    #[serde(default)]
    pub hugepages: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeIface {
    pub iface: String,
    #[serde(default)]
    pub vlan: Option<u16>,
    /// Binds the interface to a slice from `network.slices`.
    #[serde(default)]
    pub slice: Option<String>,
    /// Optional node address in CIDR form (e.g. `10.101.0.2/24`), configured
    /// by the zone backend at `zone up`.
    #[serde(default)]
    pub ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlicePolicyRule {
    /// Traffic class / DSCP / app id matcher (free-form until phase 2 pins it).
    #[serde(rename = "match")]
    pub matcher: String,
    pub slice: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub link_type: LinkType,
    #[serde(default)]
    pub properties: Vec<serde_yaml_ng::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkType {
    VlanTrunk,
    Vxlan,
    Wireguard,
    Direct,
    Can,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrafficProfile {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub pps: Option<u64>,
    #[serde(default)]
    pub bandwidth_mbps: Option<u64>,
    #[serde(default)]
    pub payload_bytes: Option<u32>,
    #[serde(default)]
    pub carla: Option<CarlaProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarlaProfile {
    pub scenario: String,
    #[serde(default)]
    pub sensors: Vec<String>,
    #[serde(default)]
    pub bridge: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telemetry {
    #[serde(default)]
    pub prometheus: Option<PrometheusCfg>,
    #[serde(default)]
    pub gpu: Option<GpuTelemetry>,
    #[serde(default)]
    pub pcap: Option<PcapCfg>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrometheusCfg {
    #[serde(default)]
    pub scrape_targets: Vec<String>,
    /// Targets that are legitimately absent under some experiment arms (a
    /// burst driver's exporter that only exists while that arm is active,
    /// say) — vxtelemetry doesn't count a failed scrape here against its
    /// completeness metric. See ARCHITECTURE.md §6.
    #[serde(default)]
    pub optional_scrape_targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpuTelemetry {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PcapCfg {
    #[serde(default)]
    pub interfaces: Vec<PcapIface>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PcapIface {
    pub name: String,
    #[serde(default)]
    pub duration_s: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Results {
    #[serde(default)]
    pub artifact_dir: Option<String>,
    #[serde(default)]
    pub upload: Option<bool>,
    #[serde(default)]
    pub upload_target: Option<String>,
}

impl ExperimentManifest {
    /// Parse YAML and run semantic validation.
    pub fn from_yaml(yaml: &str) -> Result<Self, ManifestError> {
        let manifest: Self = serde_yaml_ng::from_str(yaml)?;
        let errors = manifest.validate();
        if errors.is_empty() {
            Ok(manifest)
        } else {
            Err(ManifestError::Invalid(errors))
        }
    }

    /// Semantic checks the type system can't express: vxid uniqueness,
    /// slice references, slice-mode consistency, service references.
    /// Shared with `VehicleManifest`, which reuses the same
    /// zones/compute/network/services blocks (§6.2).
    pub fn validate(&self) -> Vec<ValidationError> {
        validate_composition(&self.zones, &self.compute, &self.network, &self.services)
    }
}

/// The vxid-uniqueness/slice-reference/service-reference checks common to
/// both manifest kinds: an experiment manifest and a vehicle manifest both
/// carry `zones`/`compute`/`network`/`services` (§6.1, §6.2) and must be
/// internally consistent the same way.
fn validate_composition(
    zones: &[Zone],
    compute: &[ComputeNode],
    network: &Option<Network>,
    services: &BTreeMap<String, ServiceDef>,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // vxid uniqueness across zones, compute, and nodes.
    let mut seen = std::collections::BTreeSet::new();
    let mut check_unique = |id: &str, errors: &mut Vec<ValidationError>| {
        if !seen.insert(id.to_string()) {
            errors.push(ValidationError::DuplicateVxid(id.to_string()));
        }
    };
    for zone in zones {
        check_unique(&zone.id, &mut errors);
        for node in &zone.nodes {
            check_unique(&node.id, &mut errors);
        }
    }
    for c in compute {
        check_unique(&c.id, &mut errors);
    }

    // Slice-mode consistency.
    let slice_ids: std::collections::BTreeSet<&str> = network
        .as_ref()
        .map(|n| n.slices.iter().map(|s| s.id.as_str()).collect())
        .unwrap_or_default();
    if let Some(network) = network {
        for slice in &network.slices {
            match slice.mode {
                SliceMode::Vlan if slice.vlan.is_none() => {
                    errors.push(ValidationError::VlanSliceWithoutVlan(slice.id.clone()));
                }
                SliceMode::Snssai if slice.s_nssai.is_none() => {
                    errors.push(ValidationError::SnssaiSliceWithoutSnssai(slice.id.clone()));
                }
                _ => {}
            }
        }
    }

    // Slice references from node ifaces and slice policies.
    for zone in zones {
        for node in &zone.nodes {
            for iface in &node.network {
                if let Some(slice) = &iface.slice
                    && !slice_ids.contains(slice.as_str())
                {
                    errors.push(ValidationError::UnknownSlice {
                        node: node.id.clone(),
                        slice: slice.clone(),
                    });
                }
            }
            for rule in &node.slice_policy {
                if !slice_ids.contains(rule.slice.as_str()) {
                    errors.push(ValidationError::UnknownSlice {
                        node: node.id.clone(),
                        slice: rule.slice.clone(),
                    });
                }
            }
            // Execution doesn't implement URSP-style multi-session routing
            // yet — a manifest that declares slice_policy and passes
            // validation would silently have it ignored at provisioning
            // time. Reject it loudly instead.
            if !node.slice_policy.is_empty() {
                errors.push(ValidationError::SlicePolicyNotImplemented(node.id.clone()));
            }
        }
    }

    // Service references from zones and compute nodes.
    let service_ids: std::collections::BTreeSet<&str> =
        services.values().map(|s| s.id.as_str()).collect();
    let check_service_refs =
        |referrer: &str, refs: &[String], errors: &mut Vec<ValidationError>| {
            for svc in refs {
                if !service_ids.contains(svc.as_str()) {
                    errors.push(ValidationError::UnknownService {
                        referrer: referrer.to_string(),
                        svc: svc.clone(),
                    });
                }
            }
        };
    for zone in zones {
        check_service_refs(&zone.id, &zone.services, &mut errors);
    }
    for c in compute {
        check_service_refs(&c.id, &c.services, &mut errors);
    }

    errors
}

/// Vehicle manifest (§6.2): composes an entire SDV in software, reusing the
/// `zones`/`compute`/`network`/`services` blocks an experiment manifest
/// also carries. An experiment manifest may reference one via
/// `experiment.vehicle` (a path); the CLI/daemon still validate this
/// document independently before composing it in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VehicleManifest {
    pub vehicle: Vehicle,
    #[serde(default)]
    pub network: Option<Network>,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceDef>,
    #[serde(default)]
    pub compute: Vec<ComputeNode>,
    #[serde(default)]
    pub zones: Vec<Zone>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vehicle {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Phase 3: COVESA VSS signal plane via the Kuksa databroker.
    #[serde(default)]
    pub signal_plane: Option<SignalPlane>,
    #[serde(default)]
    pub comms: Option<Comms>,
    /// Optional co-simulation source (CARLA/AlpaSim) driving traffic
    /// profiles with `carla:` blocks (§6.1).
    #[serde(default)]
    pub world: Option<World>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalPlane {
    pub kuksa: KuksaCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KuksaCfg {
    pub image: Image,
    /// Path to the COVESA VSS signal catalog.
    #[serde(default)]
    pub vss_catalog: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comms {
    pub ros2_rmw: Ros2Rmw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ros2Rmw {
    #[serde(rename = "rmw_zenoh")]
    RmwZenoh,
    #[serde(rename = "rmw_fastrtps")]
    RmwFastrtps,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct World {
    pub simulator: Simulator,
    #[serde(default)]
    pub image: Option<Image>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Simulator {
    #[serde(rename = "carla")]
    Carla,
    #[serde(rename = "alpasim")]
    Alpasim,
    #[serde(rename = "none")]
    None,
}

impl VehicleManifest {
    /// Parse YAML and run the same semantic validation an experiment
    /// manifest gets (§6.2 reuses §6.1's zones/compute/network/services).
    pub fn from_yaml(yaml: &str) -> Result<Self, ManifestError> {
        let manifest: Self = serde_yaml_ng::from_str(yaml)?;
        let errors = manifest.validate();
        if errors.is_empty() {
            Ok(manifest)
        } else {
            Err(ManifestError::Invalid(errors))
        }
    }

    pub fn validate(&self) -> Vec<ValidationError> {
        validate_composition(&self.zones, &self.compute, &self.network, &self.services)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
experiment:
  id: exp-001
  name: ADAS slice isolation test
  duration_s: 1800
  results_prefix: results/exp-001

network:
  slices:
    - id: adas
      mode: vlan
      vlan: 101
      qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 }
    - id: infotainment
      mode: vlan
      vlan: 103
      qos: { min_rate_mbps: 0, max_rate_mbps: 500, priority: 5 }

services:
  ota:
    id: ota-01
    type: ota
    image:
      url: registry/vxota:latest
      kind: container
    config:
      artifacts: /opt/ota/artifacts
  telemetry:
    id: telemetry-01
    type: telemetry
    image:
      url: registry/vxtelemetry:latest
      kind: container
    config:
      scrape_interval_s: 5

zones:
  - id: vxzone-front-left-01
    role: front-left
    nodes:
      - id: node-ecu-01
        type: ecu
        image: { url: registry/zephyr-probe:1, kind: rtimage }
        backend: qemu-microvm
        network:
          - iface: eth0
            vlan: 101
            slice: adas

compute:
  - id: vxcompute-central-01
    name: central
    image: { url: registry/responder:1, kind: container }
    resources: { cpu: 4, memory: 8, gpu: passthrough }
    services: [ota-01]

traffic_profiles:
  infotain-load:
    source: vxzone-front-left-01:node-ecu-01
    target: vxcompute-central-01
    pps: 10000
    bandwidth_mbps: 400
    payload_bytes: 1200
"#;

    #[test]
    fn parses_and_validates_worked_example() {
        let m = ExperimentManifest::from_yaml(EXAMPLE).expect("valid manifest");
        assert_eq!(m.experiment.id, "exp-001");
        assert_eq!(m.zones[0].role, ZoneRole::FrontLeft);
        assert_eq!(m.zones[0].nodes[0].backend, Some(Backend::QemuMicrovm));
        assert_eq!(
            m.compute[0].resources.as_ref().unwrap().gpu,
            Some(GpuMode::Passthrough)
        );
        assert_eq!(m.services["ota"].service_type, ServiceType::Ota);
    }

    #[test]
    fn rejects_unknown_slice_reference() {
        let bad = EXAMPLE.replace("slice: adas", "slice: nonexistent");
        let err = ExperimentManifest::from_yaml(&bad).unwrap_err();
        match err {
            ManifestError::Invalid(errors) => assert!(errors
                .iter()
                .any(|e| matches!(e, ValidationError::UnknownSlice { slice, .. } if slice == "nonexistent"))),
            other => panic!("expected validation error, got: {other}"),
        }
    }

    #[test]
    fn rejects_duplicate_vxids() {
        let bad = EXAMPLE.replace("vxcompute-central-01", "vxzone-front-left-01");
        let err = ExperimentManifest::from_yaml(&bad).unwrap_err();
        match err {
            ManifestError::Invalid(errors) => assert!(errors.iter().any(
                |e| matches!(e, ValidationError::DuplicateVxid(id) if id == "vxzone-front-left-01")
            )),
            other => panic!("expected validation error, got: {other}"),
        }
    }

    #[test]
    fn rejects_slice_policy_as_not_yet_implemented() {
        // slice_policy parses fine (the schema exists) but execution doesn't
        // implement URSP-style routing yet — validate() must reject it
        // loudly rather than silently accepting a no-op policy.
        let bad = EXAMPLE.replace(
            "        network:\n          - iface: eth0\n            vlan: 101\n            slice: adas\n",
            "        network:\n          - iface: eth0\n            vlan: 101\n            slice: adas\n        slice_policy:\n          - match: \"dscp:46\"\n            slice: adas\n",
        );
        let err = ExperimentManifest::from_yaml(&bad).unwrap_err();
        match err {
            ManifestError::Invalid(errors) => assert!(errors.iter().any(
                |e| matches!(e, ValidationError::SlicePolicyNotImplemented(node) if node == "node-ecu-01")
            )),
            other => panic!("expected validation error, got: {other}"),
        }
    }

    #[test]
    fn rejects_vlan_slice_without_vlan_id() {
        // Target the slice block's vlan line only (the node iface also has one).
        let bad = EXAMPLE.replace("mode: vlan\n      vlan: 101\n", "mode: vlan\n");
        let err = ExperimentManifest::from_yaml(&bad).unwrap_err();
        assert!(matches!(err, ManifestError::Invalid(_)));
    }

    #[test]
    fn rejects_unknown_fields() {
        let bad = EXAMPLE.replace("duration_s: 1800", "duration_s: 1800\n  surprise_field: 1");
        assert!(ExperimentManifest::from_yaml(&bad).is_err());
    }

    const VEHICLE_EXAMPLE: &str = r#"
vehicle:
  id: vx-vehicle-01
  name: "vx-vehicle-01 OEM-representative zonal E/E"
  description: Four-zone + central-hpc reference vehicle
  comms:
    ros2_rmw: rmw_zenoh
  world:
    simulator: carla
    image: { url: registry/carla-server:0.10, kind: container }

network:
  slices:
    - id: adas
      mode: vlan
      vlan: 101
      qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 }

services:
  telemetry:
    id: telemetry-01
    type: telemetry
    image:
      url: registry/vxtelemetry:latest
      kind: container
    config:
      scrape_interval_s: 5

zones:
  - id: vxzone-front-left-01
    role: front-left
    services: [telemetry-01]
    nodes:
      - id: node-ecu-01
        type: ecu
        image: { url: registry/zephyr-probe:1, kind: rtimage }
        backend: qemu-microvm
        network:
          - iface: eth0
            vlan: 101
            slice: adas

compute:
  - id: vxcompute-central-01
    name: central
    image: { url: registry/responder:1, kind: container }
    resources: { cpu: 4, memory: 8, gpu: passthrough }
"#;

    #[test]
    fn parses_and_validates_vehicle_manifest() {
        let m = VehicleManifest::from_yaml(VEHICLE_EXAMPLE).expect("valid vehicle manifest");
        assert_eq!(m.vehicle.id, "vx-vehicle-01");
        assert_eq!(m.vehicle.comms.unwrap().ros2_rmw, Ros2Rmw::RmwZenoh);
        assert_eq!(m.vehicle.world.unwrap().simulator, Simulator::Carla);
        assert_eq!(m.zones[0].nodes[0].backend, Some(Backend::QemuMicrovm));
        assert_eq!(m.compute[0].id, "vxcompute-central-01");
    }

    #[test]
    fn vehicle_manifest_reuses_the_same_slice_reference_check() {
        // §6.2 reuses §6.1's zones/compute/network/services blocks — a
        // vehicle manifest must fail the same way an experiment manifest
        // would for an unknown slice reference, via the shared
        // validate_composition() path.
        let bad = VEHICLE_EXAMPLE.replace("slice: adas", "slice: nonexistent");
        let err = VehicleManifest::from_yaml(&bad).unwrap_err();
        match err {
            ManifestError::Invalid(errors) => assert!(errors.iter().any(
                |e| matches!(e, ValidationError::UnknownSlice { slice, .. } if slice == "nonexistent")
            )),
            other => panic!("expected validation error, got: {other}"),
        }
    }

    #[test]
    fn vehicle_manifest_rejects_slice_policy_as_not_yet_implemented() {
        let bad = VEHICLE_EXAMPLE.replace(
            "            slice: adas\n",
            "            slice: adas\n        slice_policy:\n          - match: \"dscp:46\"\n            slice: adas\n",
        );
        let err = VehicleManifest::from_yaml(&bad).unwrap_err();
        match err {
            ManifestError::Invalid(errors) => assert!(errors.iter().any(
                |e| matches!(e, ValidationError::SlicePolicyNotImplemented(node) if node == "node-ecu-01")
            )),
            other => panic!("expected validation error, got: {other}"),
        }
    }

    #[test]
    fn vehicle_manifest_rejects_unknown_fields() {
        let bad = VEHICLE_EXAMPLE.replace(
            "  description: Four-zone + central-hpc reference vehicle",
            "  description: Four-zone + central-hpc reference vehicle\n  surprise_field: 1",
        );
        assert!(VehicleManifest::from_yaml(&bad).is_err());
    }
}
