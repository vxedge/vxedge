//! A traffic profile whose source node's zone is up runs inside that node (X3.2).
//!
//! Once a node is up, its gateway-side veth lives in its own namespace, so a generator on the
//! host has nowhere to send from. This runs the generator where the node's traffic really starts:
//! inside the node's namespace, from its pinned address on its slice. A target node is entered
//! the same way, and `internet` is the far side of the shared uplink, addressed at the first host
//! of the source's subnet, as measurements do (`crate::measurements::run`).
//!
//! Its processes use `traffic::lifecycle`'s file names (`traffic-<id>-<role>.*`), so
//! `TrafficService.Status` and `Down` treat a profile run here as any other.

use std::path::Path;

use crate::containers::LazyClient;
use crate::gateway::{GatewayPlan, Shell};
use crate::measurements::run::{
    End, Reflector, TRAFFIC, alive, end_on, ensure_far_side, far_side_of, netns_of, read_pid_of,
    spawn_as,
};
use crate::model::{Experiment, TrafficEndpoint};
use crate::zones::NodePlan;

/// The port a profile run inside a node sends to. Far from the ports node payloads use, since a
/// reflector shares its node's namespace with them.
pub const PORT: u16 = 19700;

/// Where one profile runs once its source node is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InNode {
    pub id: String,
    pub source: End,
    pub target: Reflector,
    pub pps: Option<u32>,
    pub bandwidth_mbps: Option<u32>,
    pub payload_bytes: u32,
}

/// The record a started profile leaves, so a measurement does not remove a far side it sends to.
fn record(runtime_dir: &Path, id: &str) -> std::path::PathBuf {
    runtime_dir.join(format!("traffic-{id}.in-node.json"))
}

/// Where profile `id` runs inside its nodes, or why it cannot.
pub fn plan(
    model: &Experiment,
    gateway: &GatewayPlan,
    nodes: &[NodePlan],
    id: &str,
) -> Result<InNode, String> {
    let what = format!("traffic profile `{id}`");
    let profile = model
        .traffic
        .iter()
        .find(|p| p.id == id)
        .ok_or_else(|| format!("no traffic profile `{id}`"))?;
    let TrafficEndpoint::Device(from) = &profile.source else {
        return Err(format!(
            "{what}: its source is not a node, so it cannot run inside one; bring the run up \
             with the gateway stage only (`experiment up --stage gateway`)"
        ));
    };
    let slice = model
        .entities
        .iter()
        .flat_map(|v| &v.links)
        .filter(|l| l.endpoints.iter().any(|e| &e.node == from))
        .find_map(|l| l.slice.clone())
        .ok_or_else(|| format!("{what}: `{from}` has no link on a slice"))?;
    let source = end_on(model, nodes, from, &slice, &what)?;
    let target = match &profile.target {
        TrafficEndpoint::Device(to) => Reflector::Node(end_on(model, nodes, to, &slice, &what)?),
        _ => far_side_of(model, gateway, from, &slice, &what)?,
    };
    if profile.pps.is_none() && profile.bandwidth_mbps.is_none() {
        return Err(format!(
            "{what}: it sets neither `pps` nor `bandwidth_mbps`"
        ));
    }
    Ok(InNode {
        id: id.to_string(),
        source,
        target,
        pps: profile.pps,
        bandwidth_mbps: profile.bandwidth_mbps,
        payload_bytes: profile.payload_bytes.unwrap_or(64),
    })
}

fn running(runtime_dir: &Path, id: &str, role: &str) -> bool {
    read_pid_of(TRAFFIC, runtime_dir, id, role).is_some_and(alive)
}

/// Start profile `p` for `duration_s`: a reflector for a round-trip probe (`pps`), then the
/// sender (`vxprobe send`) or the load (`vxtraffic`) inside the source node. A role already
/// running is left as it is.
#[allow(clippy::too_many_arguments)]
pub async fn up(
    sh: &Shell,
    containers: &LazyClient,
    runtime_dir: &Path,
    vxprobe_bin: &str,
    vxtraffic_bin: &str,
    p: &InNode,
    duration_s: u64,
) -> Result<String, String> {
    let err = |e: String| format!("traffic profile `{}`: {e}", p.id);
    let role = if p.pps.is_some() { "send" } else { "load" };
    if running(runtime_dir, &p.id, role) {
        return Ok(format!(
            "traffic profile `{}` already up (idempotent)",
            p.id
        ));
    }
    let source_ns = netns_of(&p.source, containers).await.map_err(err)?;
    let (target_addr, far_side) = match &p.target {
        // In-node profiles run on VLAN slices; an s-nssai profile goes through the UE path in
        // `traffic::lifecycle`.
        Reflector::Ue(session) => {
            return Err(err(format!(
                "slice `{}` is s-nssai, which an in-node profile does not run on",
                session.slice_id
            )));
        }
        Reflector::Node(to) => {
            if p.pps.is_some() && !running(runtime_dir, &p.id, "reflect") {
                let ns = netns_of(to, containers).await.map_err(err)?;
                let (rt, id, bin) = (
                    runtime_dir.to_path_buf(),
                    p.id.clone(),
                    vxprobe_bin.to_string(),
                );
                let args = vec![
                    "reflect".into(),
                    "--listen".into(),
                    format!("0.0.0.0:{PORT}"),
                ];
                tokio::task::spawn_blocking(move || {
                    spawn_as(TRAFFIC, &rt, &id, "reflect", Some(&ns), &bin, args, None)
                })
                .await
                .map_err(|e| err(e.to_string()))?
                .map_err(err)?;
            }
            (to.addr.clone(), None)
        }
        Reflector::FarSide {
            parent,
            vlan,
            iface,
            cidr,
            addr,
        } => {
            // A load needs the far-side address too: the source resolves it on its own subnet.
            let reflect = p.pps.is_some() && !running(runtime_dir, &p.id, "reflect");
            let (sh, parent, iface_c, cidr, vlan, addr_c) = (
                sh.clone(),
                parent.clone(),
                iface.clone(),
                cidr.clone(),
                *vlan,
                addr.clone(),
            );
            let (rt, id, bin) = (
                runtime_dir.to_path_buf(),
                p.id.clone(),
                vxprobe_bin.to_string(),
            );
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                ensure_far_side(&sh, &parent, vlan, &iface_c, &cidr)?;
                if reflect {
                    let args = vec![
                        "reflect".into(),
                        "--listen".into(),
                        format!("{addr_c}:{PORT}"),
                    ];
                    spawn_as(TRAFFIC, &rt, &id, "reflect", None, &bin, args, None)?;
                }
                Ok(())
            })
            .await
            .map_err(|e| err(e.to_string()))?
            .map_err(err)?;
            (addr.clone(), Some(iface.clone()))
        }
    };
    std::fs::write(
        record(runtime_dir, &p.id),
        serde_json::json!({ "far_side": far_side }).to_string(),
    )
    .map_err(|e| err(format!("cannot write its record: {e}")))?;
    let (bin, args) = match (p.pps, p.bandwidth_mbps) {
        (Some(pps), _) => {
            // The reflector binds before the first probe is sent.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            (
                vxprobe_bin,
                vec![
                    "send".to_string(),
                    "--target".into(),
                    format!("{target_addr}:{PORT}"),
                    "--pps".into(),
                    pps.to_string(),
                    "--payload-bytes".into(),
                    p.payload_bytes.to_string(),
                    "--duration-s".into(),
                    duration_s.to_string(),
                ],
            )
        }
        (None, Some(mbps)) => (
            vxtraffic_bin,
            vec![
                "--target".to_string(),
                format!("{target_addr}:{PORT}"),
                "--bandwidth-mbps".into(),
                mbps.to_string(),
                "--payload-bytes".into(),
                p.payload_bytes.to_string(),
                "--duration-s".into(),
                duration_s.to_string(),
            ],
        ),
        (None, None) => return Err(err("it sets neither `pps` nor `bandwidth_mbps`".into())),
    };
    let (rt, id, bin) = (runtime_dir.to_path_buf(), p.id.clone(), bin.to_string());
    tokio::task::spawn_blocking(move || {
        spawn_as(TRAFFIC, &rt, &id, role, Some(&source_ns), &bin, args, None)
    })
    .await
    .map_err(|e| err(e.to_string()))?
    .map_err(err)?;
    let rate = match (p.pps, p.bandwidth_mbps) {
        (Some(pps), _) => format!("{pps} pps"),
        (None, Some(mbps)) => format!("{mbps} mbit"),
        _ => String::new(),
    };
    Ok(format!(
        "traffic profile `{}` up inside `{}` ({rate} to {target_addr}:{PORT}, {duration_s}s)",
        p.id, p.source.node
    ))
}

/// Forget profile `id`'s in-node record. Its processes are stopped by `lifecycle::profile_down`.
pub fn forget(runtime_dir: &Path, id: &str) {
    let _ = std::fs::remove_file(record(runtime_dir, id));
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = r#"
kind: vehicle
id: car
zones: { front: {}, rear: {} }
devices:
  probe1: { role: zone-controller, zone: front, backend: container }
  load1: { role: zone-controller, zone: rear, backend: container }
functions:
  f1: { deployed_on: probe1 }
  f2: { deployed_on: load1 }
links:
  - { id: l1, endpoints: [{ node: probe1, port: eth0, ip: 10.101.0.2/24 }], slice: adas }
  - { id: l2, endpoints: [{ node: load1, port: eth0, ip: 10.103.0.2/24 }], slice: info }
---
kind: baseline
id: bl
parts:
  - { ref: 'local/vxprobe:host-process', target_ids: [f1, f2] }
---
kind: experiment
id: exp
vehicles: [car]
baseline: bl
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
    - { id: info, mode: vlan, vlan: 103 }
traffic_profiles:
  bg: { source: load1, target: internet, bandwidth_mbps: 400, payload_bytes: 1200 }
  rtt: { source: probe1, target: internet, pps: 500 }
"#;

    fn plan_of(id: &str) -> Result<InNode, String> {
        let model = crate::model::load_yaml(WS).unwrap();
        let gateway = GatewayPlan::from_model(&model).unwrap();
        let nodes =
            crate::zones::plan_nodes_from_model(&model, &gateway, Path::new("/nonexistent"))
                .unwrap();
        plan(&model, &gateway, &nodes, id)
    }

    #[test]
    fn a_load_to_the_uplink_sends_from_its_node_to_the_far_side_of_its_slice() {
        let p = plan_of("bg").unwrap();
        assert_eq!(
            (p.source.node.as_str(), p.source.addr.as_str()),
            ("load1", "10.103.0.2")
        );
        assert_eq!((p.bandwidth_mbps, p.payload_bytes), (Some(400), 1200));
        let Reflector::FarSide {
            vlan, addr, cidr, ..
        } = &p.target
        else {
            panic!("{:?}", p.target)
        };
        assert_eq!(
            (*vlan, addr.as_str(), cidr.as_str()),
            (103, "10.103.0.1", "10.103.0.1/24")
        );
    }

    #[test]
    fn a_probe_profile_plans_a_far_side_reflector() {
        let p = plan_of("rtt").unwrap();
        assert_eq!(p.pps, Some(500));
        assert_eq!(p.target.addr(), Some("10.101.0.1"));
    }

    #[test]
    fn an_unknown_profile_is_named() {
        assert!(plan_of("nope").unwrap_err().contains("nope"));
    }

    #[test]
    fn nothing_running_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!running(dir.path(), "bg", "load"));
        forget(dir.path(), "bg");
    }
}
