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
    };

    out.push(record(HOST.to_string(), Kind::Host));
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
        let json = serde_json::to_value(ecu).unwrap();
        assert_eq!(json["kind"], "node");
        assert_eq!(json["payloads"][0]["ref"], "ghcr.io/vxedge/parts/vxprobe");
    }
}
