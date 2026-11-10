//! **Entities** (`ARCHITECTURE.md` §12): the path every telemetry record names, derived from the
//! model once, so a sample, a log line and an event about the same thing agree on what it is.
//!
//! ```text
//! <vehicle>/<zone>/<node>      a device in a zone
//! <vehicle>/_/<node>           a device the vehicle declares in no zone (a gateway, central compute)
//! <vehicle>/links/<link>       a link
//! <vehicle>/channels/<channel> a channel
//! <vehicle>/slices/<slice>     a slice the vehicle's links use
//! <vehicle>/services/<service> a service placed on one of the vehicle's devices
//! host/services/<service>      a service run as a host process
//! host                         the testbed host, including the observer's own cost
//! measurements/<measurement>   a declared measurement
//! ```
//!
//! Paths come from ids, never from addresses. A scrape target is bound to the node whose link
//! endpoint carries its address, and to `host` otherwise, so mapping a target to an entity is
//! the daemon's job and is done once.

use serde::Serialize;

use crate::model::{DeviceRole, Experiment};

/// What an entity is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Node,
    Link,
    Channel,
    Slice,
    Service,
    Host,
    Measurement,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Link => "link",
            Self::Channel => "channel",
            Self::Slice => "slice",
            Self::Service => "service",
            Self::Host => "host",
            Self::Measurement => "measurement",
        }
    }
}

/// A payload running on a node, as the documents pin it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Payload {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// One entity, as the `entities` record lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntityRecord {
    pub path: String,
    pub kind: Kind,
    /// A node's role (`zone-controller`, `hpc`, …), a service's type (`ota`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// A node's backend, as the manifest spells it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// What runs on a node: each function's part.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub payloads: Vec<Payload>,
    /// The telemetry scrape targets bound to this entity.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub scrape_targets: Vec<String>,
    /// The log files this daemon writes for the entity, which the ingest reads into `log`
    /// records (decision X-18). Filled by [`add_logs`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<LogSource>,
}

/// One log file of an entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogSource {
    /// What the file is to its entity: `serial` (a guest's console), `vmm` (the hypervisor's own
    /// log), `stdout` (a capsule runner's or a host service's output), `attach` (an attached
    /// platform's scripts), or `traffic-<profile>-<role>`.
    pub stream: String,
    pub path: std::path::PathBuf,
}

/// The path of the host entity.
pub const HOST: &str = "host";

fn role_str(role: DeviceRole) -> &'static str {
    match role {
        DeviceRole::ZoneController => "zone-controller",
        DeviceRole::Hpc => "hpc",
        DeviceRole::Gateway => "gateway",
        DeviceRole::Ecu => "ecu",
        DeviceRole::Sensor => "sensor",
        DeviceRole::Actuator => "actuator",
        DeviceRole::Switch => "switch",
    }
}

/// The path of device `node` of vehicle `vehicle`, or `None` when the model has no such device.
pub fn node_path(model: &Experiment, node: &str) -> Option<String> {
    model.entities.iter().find_map(|v| {
        v.devices
            .get(node)
            .map(|d| format!("{}/{}/{}", v.id, d.zone.as_deref().unwrap_or("_"), d.id))
    })
}

/// The host part of a `host:port` scrape target.
fn target_host(target: &str) -> &str {
    let t = target
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let t = t.split('/').next().unwrap_or(t);
    match t.rsplit_once(':') {
        Some((h, _)) => h.trim_start_matches('[').trim_end_matches(']'),
        None => t,
    }
}

/// The entity a scrape target belongs to: the node whose link endpoint has its address, or
/// [`HOST`].
pub fn target_entity(model: &Experiment, target: &str) -> String {
    let host = target_host(target);
    for vehicle in &model.entities {
        for link in &vehicle.links {
            for e in &link.endpoints {
                let mut addrs = e.ip_cidr.iter().chain(e.extra_cidrs.iter());
                if addrs.any(|c| c.split('/').next() == Some(host))
                    && let Some(path) = node_path(model, &e.node)
                {
                    return path;
                }
            }
        }
    }
    HOST.to_string()
}

/// Every entity of the model, in a stable order: host first, then each vehicle's nodes, links,
/// channels, slices and services, by id.
pub fn entities_of(model: &Experiment) -> Vec<EntityRecord> {
    entities_with(model, &[])
}

/// [`entities_of`], with `host_targets` bound to `host` as well: the daemon's own exporter,
/// whose samples each name their entity with an `entity` label.
pub fn entities_with(model: &Experiment, host_targets: &[String]) -> Vec<EntityRecord> {
    let mut out = Vec::new();
    let targets: Vec<(String, String)> = model
        .telemetry
        .scrape_targets
        .iter()
        .chain(&model.telemetry.optional_scrape_targets)
        .map(|t| (target_entity(model, t), t.clone()))
        .collect();
    let bound = |path: &str| -> Vec<String> {
        targets
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, t)| t.clone())
            .collect()
    };
    let record = |path: String, kind| EntityRecord {
        scrape_targets: bound(&path),
        path,
        kind,
        role: None,
        backend: None,
        payloads: Vec::new(),
        logs: Vec::new(),
    };

    let mut host = record(HOST.to_string(), Kind::Host);
    let measurements = model.measurements.iter().map(|m| {
        let mut r = record(m.entity(), Kind::Measurement);
        r.role = Some(m.kind.as_str().to_string());
        r
    });
    for t in host_targets {
        if !host.scrape_targets.contains(t) {
            host.scrape_targets.push(t.clone());
        }
    }
    out.push(host);
    out.extend(measurements);
    for (id, def) in &model.services {
        if def.on.is_none() {
            let mut r = record(format!("{HOST}/services/{id}"), Kind::Service);
            r.role = Some(def.service_type.as_str().to_string());
            out.push(r);
        }
    }
    for vehicle in &model.entities {
        for device in vehicle.devices.values() {
            let path = format!(
                "{}/{}/{}",
                vehicle.id,
                device.zone.as_deref().unwrap_or("_"),
                device.id
            );
            let mut r = record(path, Kind::Node);
            r.role = Some(role_str(device.role).to_string());
            r.backend = device.backend.map(|b| b.as_str().to_string());
            r.payloads = vehicle
                .functions_on(&device.id)
                .filter_map(|f| f.part.as_ref())
                .map(|p| Payload {
                    reference: p.reference.clone(),
                    digest: p.digest.clone(),
                })
                .collect();
            out.push(r);
        }
        for link in &vehicle.links {
            out.push(record(
                format!("{}/links/{}", vehicle.id, link.id),
                Kind::Link,
            ));
        }
        for channel in &vehicle.channels {
            out.push(record(
                format!("{}/channels/{}", vehicle.id, channel.id),
                Kind::Channel,
            ));
        }
        for slice in &model.slices {
            if vehicle
                .links
                .iter()
                .any(|l| l.slice.as_deref() == Some(&slice.id))
            {
                out.push(record(
                    format!("{}/slices/{}", vehicle.id, slice.id),
                    Kind::Slice,
                ));
            }
        }
        for (id, def) in &model.services {
            if def
                .on
                .as_deref()
                .is_some_and(|on| vehicle.devices.contains_key(on))
            {
                let mut r = record(format!("{}/services/{id}", vehicle.id), Kind::Service);
                r.role = Some(def.service_type.as_str().to_string());
                out.push(r);
            }
        }
    }
    out
}

/// Bind the log files the daemon writes under `runtime_dir` to the entities in `records`: each
/// node's by its backend, each host service's output, and each traffic generator's output to
/// the node it runs from (a reflector to the node it answers on), or to `host` when that is no
/// device. A container's output is Podman's and has no file here.
pub fn add_logs(records: &mut [EntityRecord], model: &Experiment, runtime_dir: &std::path::Path) {
    use crate::model::Backend;
    let file = |name: String| runtime_dir.join(name);
    let mut add = |path: &str, stream: String, at: std::path::PathBuf| {
        if let Some(r) = records.iter_mut().find(|r| r.path == path) {
            r.logs.push(LogSource { stream, path: at });
        }
    };
    for vehicle in &model.entities {
        for device in vehicle.devices.values() {
            let Some(path) = node_path(model, &device.id) else {
                continue;
            };
            let name = crate::zones::runtime_name_of(&device.id);
            let streams: Vec<(&str, std::path::PathBuf)> = match device.backend {
                Some(Backend::QemuMicrovm | Backend::QemuMcu) => {
                    vec![("serial", file(format!("{name}.serial.log")))]
                }
                Some(Backend::CloudHypervisor) => vec![
                    ("serial", file(format!("{name}.serial.log"))),
                    ("vmm", file(format!("{name}.ch.log"))),
                ],
                // The domain XML points the console at this file; libvirt writes it.
                Some(Backend::Libvirt) => vec![(
                    "serial",
                    std::path::PathBuf::from(format!("/tmp/{name}.serial.log")),
                )],
                Some(Backend::WasmCapsule) => vec![("stdout", file(format!("{name}.capsule.log")))],
                Some(Backend::Attach) => vec![("attach", file(format!("{name}.attach.log")))],
                Some(Backend::Container) | None => Vec::new(),
            };
            for (stream, at) in streams {
                add(&path, stream.to_string(), at);
            }
        }
    }
    for (id, def) in &model.services {
        if def.on.is_none() {
            let t = def.service_type.as_str();
            add(
                &format!("{HOST}/services/{id}"),
                "stdout".into(),
                file(format!("svc-{t}.log")),
            );
        }
    }
    let node_of = |e: &crate::model::TrafficEndpoint| match e {
        crate::model::TrafficEndpoint::Device(d) => node_path(model, d),
        _ => None,
    };
    for p in &model.traffic {
        for (role, end) in [
            ("send", &p.source),
            ("load", &p.source),
            ("reflect", &p.target),
        ] {
            let path = node_of(end).unwrap_or_else(|| HOST.to_string());
            add(
                &path,
                format!("traffic-{}-{role}", p.id),
                file(format!("traffic-{}-{role}.log", p.id)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKSPACE: &str = r#"
kind: vehicle
id: car
zones: { front: {} }
devices:
  ecu1: { role: zone-controller, zone: front, backend: container }
  hpc1: { role: hpc, backend: qemu-microvm }
functions:
  f1: { deployed_on: ecu1 }
links:
  - { id: l1, endpoints: [{ node: ecu1, port: eth0, ip: 10.1.0.2/24 }], slice: adas }
  - { id: l2, endpoints: [{ node: hpc1, port: eth0, ip: 10.1.0.3/24 }], slice: adas }
---
kind: baseline
id: bl
parts:
  - { ref: 'ghcr.io/vxedge/parts/vxprobe', digest: 'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', kind: container, target_ids: f1 }
---
kind: experiment
id: e1
vehicles: [car]
baseline: bl
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
    - { id: unused, mode: vlan, vlan: 102 }
telemetry:
  prometheus:
    scrape_targets: ["10.1.0.2:9801", "127.0.0.1:9900"]
"#;

    fn model() -> Experiment {
        crate::model::load_workspace_yaml(WORKSPACE).unwrap()
    }

    #[test]
    fn every_entity_has_a_path_from_ids() {
        let paths: Vec<String> = entities_of(&model()).into_iter().map(|e| e.path).collect();
        assert_eq!(
            paths,
            [
                "host",
                "car/front/ecu1",
                "car/_/hpc1",
                "car/links/l1",
                "car/links/l2",
                "car/slices/adas"
            ]
        );
    }

    #[test]
    fn a_target_binds_to_the_node_that_has_its_address() {
        let m = model();
        assert_eq!(target_entity(&m, "10.1.0.2:9801"), "car/front/ecu1");
        assert_eq!(
            target_entity(&m, "http://10.1.0.3:9801/metrics"),
            "car/_/hpc1"
        );
        assert_eq!(target_entity(&m, "127.0.0.1:9900"), "host");
        let all = entities_of(&m);
        let ecu = all.iter().find(|e| e.path == "car/front/ecu1").unwrap();
        assert_eq!(ecu.scrape_targets, ["10.1.0.2:9801"]);
        assert_eq!(ecu.role.as_deref(), Some("zone-controller"));
        assert_eq!(ecu.backend.as_deref(), Some("container"));
        assert_eq!(
            ecu.payloads[0].digest.as_deref(),
            Some("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        let host = &all[0];
        assert_eq!(host.scrape_targets, ["127.0.0.1:9900"]);
        let with = entities_with(&m, &["127.0.0.1:9469".to_string()]);
        assert_eq!(with[0].scrape_targets, ["127.0.0.1:9900", "127.0.0.1:9469"]);
        let json = serde_json::to_value(ecu).unwrap();
        assert_eq!(json["kind"], "node");
        assert_eq!(json["payloads"][0]["ref"], "ghcr.io/vxedge/parts/vxprobe");
    }

    #[test]
    fn log_files_bind_to_the_entity_they_are_about() {
        let mut m = model();
        m.traffic.push(crate::model::TrafficProfile {
            id: "bulk".into(),
            source: crate::model::TrafficEndpoint::Device("ecu1".into()),
            target: crate::model::TrafficEndpoint::Internet,
            pps: None,
            bandwidth_mbps: None,
            payload_bytes: None,
        });
        let rt = std::path::Path::new("/run/vx");
        let mut all = entities_of(&m);
        add_logs(&mut all, &m, rt);
        let logs = |path: &str| -> Vec<(String, std::path::PathBuf)> {
            all.iter()
                .find(|e| e.path == path)
                .unwrap()
                .logs
                .iter()
                .map(|l| (l.stream.clone(), l.path.clone()))
                .collect()
        };
        assert_eq!(
            logs("car/_/hpc1"),
            [("serial".into(), rt.join("vx-hpc1.serial.log"))]
        );
        // A container's output is Podman's; only its traffic generator's is a file.
        assert_eq!(
            logs("car/front/ecu1"),
            [
                ("traffic-bulk-send".into(), rt.join("traffic-bulk-send.log")),
                ("traffic-bulk-load".into(), rt.join("traffic-bulk-load.log")),
            ]
        );
        assert_eq!(
            logs("host"),
            [(
                "traffic-bulk-reflect".into(),
                rt.join("traffic-bulk-reflect.log")
            )]
        );
        let json = serde_json::to_value(&all[0]).unwrap();
        assert_eq!(json["logs"][0]["stream"], "traffic-bulk-reflect");
        assert!(serde_json::to_value(&all[3]).unwrap().get("logs").is_none());
    }
}
