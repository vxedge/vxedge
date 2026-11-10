//! **Measurements** (`ARCHITECTURE.md` §6.0.3): what an experiment measures, declared in its
//! `measurements` block by node id and run by the daemon, never by a script.
//!
//! ```yaml
//! measurements:
//!   ctl-rtt:
//!     kind: rtt-probe        # the one kind so far
//!     from: ecu1             # a device id: the node the probe sends from
//!     to: internet           # a device id, or `internet`: the far side of the shared uplink
//!     slice: adas            # the slice the probe's packets travel on
//!     rate_pps: 500          # default 100
//!     payload_bytes: 64      # default 64
//! ```
//!
//! [`parse`] reads the block's shape when the model is built. [`problems`] checks it against the
//! model and returns each problem typed as a measurement problem, so `Plan` lists them all and
//! `up` refuses on the first, before anything runs. A probe runs inside its node's network
//! namespace, so a node that has none on the host (a VM, an attached platform) cannot be an end.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_yaml_ng::Value;

use crate::intent::{Problem, ProblemKind};
use crate::model::{Backend, Experiment};

pub mod run;

/// The far side of the shared uplink, as a measurement's `to`.
pub const INTERNET: &str = "internet";
/// The smallest probe: `vxprobe`'s own header.
pub const MIN_PAYLOAD_BYTES: u32 = 8;
/// The largest probe that fits one UDP datagram in a 1500-byte MTU.
pub const MAX_PAYLOAD_BYTES: u32 = 1472;
/// The fastest probe rate a measurement may ask for.
pub const MAX_RATE_PPS: u32 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Paced UDP probes echoed by a reflector: round-trip time, jitter and loss.
    RttProbe,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RttProbe => "rtt-probe",
        }
    }
}

/// One declared measurement.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measurement {
    /// Its key in the block.
    #[serde(skip)]
    pub id: String,
    pub kind: Kind,
    pub from: String,
    pub to: String,
    pub slice: String,
    #[serde(default = "default_rate_pps")]
    pub rate_pps: u32,
    #[serde(default = "default_payload_bytes")]
    pub payload_bytes: u32,
}

fn default_rate_pps() -> u32 {
    100
}

fn default_payload_bytes() -> u32 {
    64
}

impl Measurement {
    /// The entity its samples name (§12).
    pub fn entity(&self) -> String {
        format!("measurements/{}", self.id)
    }
}

/// The `measurements` block's shape, in id order, or why it has none. Absent is empty.
pub fn parse(block: Option<&Value>) -> Result<Vec<Measurement>, String> {
    let Some(block) = block else {
        return Ok(Vec::new());
    };
    let map: BTreeMap<String, Measurement> = serde_yaml_ng::from_value(block.clone())
        .map_err(|e| format!("{e} (a map of measurement id to measurement)"))?;
    Ok(map
        .into_iter()
        .map(|(id, m)| Measurement { id, ..m })
        .collect())
}

/// Every problem with the experiment's measurements, checked against its model.
pub fn problems(model: &Experiment) -> Vec<Problem> {
    let mut out = Vec::new();
    for m in &model.measurements {
        let mut push = |field: &str, detail: String| {
            out.push(Problem {
                kind: ProblemKind::Measurement,
                experiment: model.id.clone(),
                field: format!("measurements.{}.{field}", m.id),
                detail,
            })
        };
        if !model.slices.iter().any(|s| s.id == m.slice) {
            push(
                "slice",
                format!("`{}` is not a slice in `network.slices`", m.slice),
            );
        }
        if m.from == INTERNET {
            push(
                "from",
                "a probe sends from a node: `internet` can only be the `to` end".into(),
            );
        } else {
            end_problems(model, &m.from, &m.slice, "from", &mut push);
        }
        if m.to == m.from {
            push("to", format!("`{}` is also the `from` end", m.to));
        } else if m.to != INTERNET {
            end_problems(model, &m.to, &m.slice, "to", &mut push);
        }
        if !(1..=MAX_RATE_PPS).contains(&m.rate_pps) {
            push(
                "rate_pps",
                format!("{} is outside 1 to {MAX_RATE_PPS}", m.rate_pps),
            );
        }
        if !(MIN_PAYLOAD_BYTES..=MAX_PAYLOAD_BYTES).contains(&m.payload_bytes) {
            push(
                "payload_bytes",
                format!(
                    "{} is outside {MIN_PAYLOAD_BYTES} to {MAX_PAYLOAD_BYTES}",
                    m.payload_bytes
                ),
            );
        }
    }
    shared_far_sides(model, &mut out);
    out
}

/// Two far sides on different slices with one address would put that address on two VLAN
/// interfaces of the host, and the host answers both through one of them: one probe's replies
/// would be lost without anything saying why. A measurement to `internet` is checked against
/// every other one and against every traffic profile that sends from a node to a far side, since
/// such a profile runs inside its node too once the node is up.
fn shared_far_sides(model: &Experiment, out: &mut Vec<Problem>) {
    let far_of = |from: &str, slice: &str| {
        let cidr = model
            .entities
            .iter()
            .find_map(|v| endpoint_on(v, from, slice))?
            .ip_cidr
            .clone()?;
        run::first_host(&cidr)
    };
    let measured: Vec<(&Measurement, String)> = model
        .measurements
        .iter()
        .filter(|m| m.to == INTERNET)
        .filter_map(|m| Some((m, far_of(&m.from, &m.slice)?)))
        .collect();
    let loads: Vec<(String, String, String)> = model
        .traffic
        .iter()
        .filter_map(|p| {
            let crate::model::TrafficEndpoint::Device(from) = &p.source else {
                return None;
            };
            if matches!(p.target, crate::model::TrafficEndpoint::Device(_)) {
                return None;
            }
            let slice = model
                .entities
                .iter()
                .flat_map(|v| &v.links)
                .filter(|l| l.endpoints.iter().any(|e| &e.node == from))
                .find_map(|l| l.slice.clone())?;
            let addr = far_of(from, &slice)?;
            Some((format!("traffic profile `{}`", p.id), slice, addr))
        })
        .collect();
    for (i, (a, addr)) in measured.iter().enumerate() {
        let earlier = measured[..i].iter().map(|(b, other)| {
            (
                format!("measurement `{}`", b.id),
                b.slice.clone(),
                other.clone(),
            )
        });
        let clash = earlier
            .chain(loads.iter().cloned())
            .find(|(_, slice, other)| other == addr && *slice != a.slice);
        if let Some((who, slice, _)) = clash {
            out.push(Problem {
                kind: ProblemKind::Measurement,
                experiment: model.id.clone(),
                field: format!("measurements.{}.to", a.id),
                detail: format!(
                    "its far side is {addr} on slice `{}`, and {who} puts {addr} on slice \
                     `{slice}`: one host address cannot answer on two VLANs. Run one of them at \
                     a time",
                    a.slice
                ),
            });
        }
    }
}

/// A node end: a device of one of the vehicles, with a namespace on the host a probe can enter,
/// and a link on the measurement's slice.
fn end_problems(
    model: &Experiment,
    node: &str,
    slice: &str,
    field: &str,
    push: &mut impl FnMut(&str, String),
) {
    let Some((vehicle, device)) = model
        .entities
        .iter()
        .find_map(|v| v.devices.get(node).map(|d| (v, d)))
    else {
        let also = if field == "to" { ", or `internet`" } else { "" };
        push(
            field,
            format!("`{node}` is not a device of the experiment's vehicles{also}"),
        );
        return;
    };
    if let Some(
        b @ (Backend::QemuMicrovm
        | Backend::Libvirt
        | Backend::CloudHypervisor
        | Backend::Attach
        | Backend::QemuMcu),
    ) = device.backend
    {
        push(
            field,
            format!(
                "`{node}` runs on {}, and a probe cannot run inside a VM or an attached \
                 platform yet: measure from a container, capsule or bare node",
                b.as_str()
            ),
        );
    }
    match endpoint_on(vehicle, node, slice) {
        None => push("slice", format!("`{node}` has no link on slice `{slice}`")),
        Some(e) if e.ip_cidr.is_none() => push(
            field,
            format!(
                "`{node}`'s link on slice `{slice}` pins no `ip`, and a probe needs the address \
                 it sends from or answers on"
            ),
        ),
        Some(_) => {}
    }
}

/// `node`'s link endpoint on `slice`, in `vehicle`.
pub(crate) fn endpoint_on<'a>(
    vehicle: &'a crate::model::Entity,
    node: &str,
    slice: &str,
) -> Option<&'a crate::model::Endpoint> {
    vehicle
        .links
        .iter()
        .filter(|l| l.slice.as_deref() == Some(slice))
        .find_map(|l| l.endpoints.iter().find(|e| e.node == node))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VEHICLE: &str = r#"
kind: vehicle
id: car
zones: { front: {}, rear: {} }
devices:
  ecu1: { role: zone-controller, zone: front, backend: container }
  cap1: { role: sensor, zone: front, backend: wasm-capsule }
  vm1: { role: zone-controller, zone: rear, backend: qemu-microvm }
  ecu2: { role: zone-controller, zone: rear, backend: container }
functions:
  f1: { deployed_on: ecu1 }
links:
  - { id: l1, endpoints: [{ node: ecu1, port: eth0, ip: 10.1.0.2/24 }], slice: adas }
  - { id: l2, endpoints: [{ node: cap1, port: eth0, ip: 10.1.0.3/24 }], slice: adas }
  - { id: l3, endpoints: [{ node: vm1, port: eth0, ip: 10.1.0.4/24 }], slice: adas }
  - { id: l4, endpoints: [{ node: ecu2, port: eth0, ip: 10.2.0.2/24 }], slice: info }
---
kind: baseline
id: bl
parts:
  - { ref: 'local/vxprobe:host-process', target_ids: f1 }
---
kind: experiment
id: exp
vehicles: [car]
baseline: bl
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
    - { id: info, mode: vlan, vlan: 103 }
"#;

    fn model(measurements: &str) -> Experiment {
        crate::model::load_yaml(&format!("{VEHICLE}measurements:\n{measurements}")).unwrap()
    }

    fn fields(model: &Experiment) -> Vec<String> {
        problems(model).into_iter().map(|p| p.field).collect()
    }

    #[test]
    fn a_probe_from_a_container_or_capsule_to_the_uplink_or_a_node_holds() {
        let m = model(
            "  up: { kind: rtt-probe, from: ecu1, to: internet, slice: adas, rate_pps: 500 }\n  \
             peer: { kind: rtt-probe, from: cap1, to: ecu1, slice: adas }\n",
        );
        assert_eq!(fields(&m), Vec::<String>::new());
        let peer = m.measurements.iter().find(|x| x.id == "peer").unwrap();
        assert_eq!((peer.rate_pps, peer.payload_bytes), (100, 64));
        assert_eq!(peer.entity(), "measurements/peer");
    }

    #[test]
    fn each_wrong_end_slice_and_bound_is_a_typed_problem() {
        let m = model(
            "  a: { kind: rtt-probe, from: vm1, to: internet, slice: adas }\n  \
             b: { kind: rtt-probe, from: nope, to: ecu1, slice: adas }\n  \
             c: { kind: rtt-probe, from: ecu2, to: internet, slice: adas }\n  \
             d: { kind: rtt-probe, from: ecu1, to: ecu1, slice: nope, rate_pps: 0, payload_bytes: 4 }\n  \
             e: { kind: rtt-probe, from: internet, to: ecu1, slice: adas }\n",
        );
        let p = problems(&m);
        assert!(p.iter().all(|p| p.kind == ProblemKind::Measurement));
        assert_eq!(
            fields(&m),
            [
                "measurements.a.from",
                "measurements.b.from",
                "measurements.c.slice",
                "measurements.d.slice",
                "measurements.d.slice",
                "measurements.d.to",
                "measurements.d.rate_pps",
                "measurements.d.payload_bytes",
                "measurements.e.from",
            ]
        );
        assert!(p[0].detail.contains("qemu-microvm"), "{}", p[0].detail);
    }

    #[test]
    fn two_far_sides_on_one_subnet_and_different_slices_are_refused() {
        let shared = crate::model::load_yaml(
            &format!(
                "{VEHICLE}measurements:\n  a: {{ kind: rtt-probe, from: ecu1, to: internet, slice: adas }}\n  \
                 b: {{ kind: rtt-probe, from: ecu2, to: internet, slice: info }}\n"
            )
            .replace("ip: 10.2.0.2/24", "ip: 10.1.0.9/24"),
        )
        .unwrap();
        let p = problems(&shared);
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(p[0].field, "measurements.b.to");
        assert!(p[0].detail.contains("10.1.0.1"), "{}", p[0].detail);
    }

    #[test]
    fn a_far_side_a_traffic_profile_takes_on_another_slice_is_refused() {
        let m = crate::model::load_yaml(
            &format!(
                "{VEHICLE}measurements:\n  a: {{ kind: rtt-probe, from: ecu1, to: internet, slice: adas }}\n\
                 traffic_profiles:\n  bg: {{ source: ecu2, target: internet, bandwidth_mbps: 10 }}\n"
            )
            .replace("ip: 10.2.0.2/24", "ip: 10.1.0.9/24"),
        )
        .unwrap();
        let p = problems(&m);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(
            p[0].detail.contains("traffic profile `bg`"),
            "{}",
            p[0].detail
        );
    }

    #[test]
    fn an_end_with_no_pinned_address_is_refused() {
        let m = crate::model::load_yaml(
            &format!(
                "{VEHICLE}measurements:\n  a: {{ kind: rtt-probe, from: ecu1, to: internet, slice: adas }}\n"
            )
            .replace("{ node: ecu1, port: eth0, ip: 10.1.0.2/24 }", "ecu1:eth0"),
        )
        .unwrap();
        assert_eq!(fields(&m), ["measurements.a.from"]);
    }

    #[test]
    fn an_unknown_kind_or_field_is_refused_when_the_model_is_built() {
        let kind = crate::model::load_yaml(&format!(
            "{VEHICLE}measurements:\n  a: {{ kind: ping, from: ecu1, to: internet, slice: adas }}\n"
        ));
        assert!(kind.is_err());
        let field = crate::model::load_yaml(&format!(
            "{VEHICLE}measurements:\n  a: {{ kind: rtt-probe, from: ecu1, to: internet, slice: adas, pps: 5 }}\n"
        ));
        assert!(field.unwrap_err().to_string().contains("measurements"));
    }

    #[test]
    fn a_parameter_binds_a_measurement_field() {
        let doc: Value = serde_yaml_ng::from_str(
            "id: exp\nmeasurements:\n  ctl-rtt: { kind: rtt-probe, from: ecu1, to: internet, slice: adas, rate_pps: 500 }\n\
             parameters:\n  probe_pps: { type: integer, unit: 1/s, default: 500, min: 10, max: 5000, binds: \"measurements[ctl-rtt].rate_pps\" }\n",
        )
        .unwrap();
        assert_eq!(crate::intent::check(&doc), Vec::new());
    }
}
