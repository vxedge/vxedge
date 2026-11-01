//! What is running on this host that a workspace does not declare.
//!
//! The daemon's memory and its record of the active workspace both belong to one runtime
//! directory, so a daemon started with another one knows neither. The host itself still shows
//! a running vehicle: its gateway ports, and its guests. Reading those needs no privilege.

use std::collections::BTreeSet;

use super::plan::NodePlan;

const ACCESS_PREFIX: &str = "vz-";
const GUEST_MARKER: &str = "vx.node=";

/// Host objects belonging to no node in `nodes`, named for an error message. Sorted, and empty
/// when the host shows only this workspace or nothing.
pub fn foreign_objects(nodes: &[NodePlan]) -> Vec<String> {
    let ports = std::fs::read_dir("/sys/class/net")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok());
    foreign(ports, guests().map(|(_, node)| node), nodes)
}

/// Every VMM on this host booted by vxedge, as `(pid, node)`, found through `/proc`, so with no
/// runtime dir to depend on.
pub(super) fn guests() -> impl Iterator<Item = (i32, String)> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let pid = e.file_name().to_str()?.parse().ok()?;
            let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
            Some((pid, guest_node(&cmdline)?))
        })
}

fn foreign(
    links: impl Iterator<Item = String>,
    guests: impl Iterator<Item = String>,
    nodes: &[NodePlan],
) -> Vec<String> {
    let own_ports: BTreeSet<&str> = nodes.iter().map(|n| n.endpoint.as_str()).collect();
    let own_nodes: BTreeSet<&str> = nodes.iter().map(|n| n.node_id.as_str()).collect();
    let ports = links
        .filter(|l| l.starts_with(ACCESS_PREFIX) && !own_ports.contains(l.as_str()))
        .map(|l| format!("port {l}"));
    let guests = guests
        .filter(|g| !own_nodes.contains(g.as_str()))
        .map(|g| format!("guest {g}"));
    let found: BTreeSet<String> = ports.chain(guests).collect();
    found.into_iter().collect()
}

/// The node a NUL-separated `/proc/<pid>/cmdline` is the VMM of, if it is one. A shell that merely
/// mentions the marker is not: only the kernel command line handed to a VMM counts.
fn guest_node(cmdline: &[u8]) -> Option<String> {
    let cmdline = String::from_utf8_lossy(cmdline);
    let mut args = cmdline.split('\0');
    let bin = args.next()?;
    if !bin.contains("qemu") && !bin.contains("cloud-hypervisor") {
        return None;
    }
    args.flat_map(|arg| arg.split(' '))
        .find_map(|word| word.strip_prefix(GUEST_MARKER))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::GatewayPlan;

    const WS: &str = r#"
kind: vehicle
id: car
zones: { front: {} }
devices:
  ecu1: { role: ecu, zone: front, backend: container }
functions:
  f: { deployed_on: ecu1 }
links:
  - { id: l1, endpoints: [{ node: ecu1, port: eth0, ip: 10.1.0.2/24 }], slice: adas }
---
kind: baseline
id: bl
parts:
  - { ref: 'local/vxprobe:host-process', target_ids: f }
"#;

    fn nodes() -> Vec<NodePlan> {
        let model = crate::model::load_vehicle_yaml(WS).unwrap();
        let gateway = GatewayPlan::from_model(&model).unwrap();
        crate::zones::plan_nodes_from_model(&model, &gateway, std::path::Path::new(".")).unwrap()
    }

    fn strings(names: &[&str]) -> impl Iterator<Item = String> {
        names
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn a_host_showing_only_this_workspace_has_nothing_foreign() {
        let links = strings(&["lo", "docker0", "vxbr0", "vz-ecu1", "vzp-ecu1"]);
        assert!(foreign(links, strings(&["ecu1"]), &nodes()).is_empty());
    }

    #[test]
    fn another_vehicles_port_and_guest_are_named() {
        let links = strings(&["vxbr0", "vz-ecu1", "vz-node-a"]);
        assert_eq!(
            foreign(links, strings(&["node-a", "node-a"]), &nodes()),
            ["guest node-a", "port vz-node-a"]
        );
    }

    #[test]
    fn only_a_vmm_counts_as_a_guest() {
        let qemu = b"qemu-system-x86_64\0-append\0console=ttyS0 vx.node=zone-fl\0";
        assert_eq!(guest_node(qemu).as_deref(), Some("zone-fl"));
        let ch = b"/usr/bin/cloud-hypervisor\0--cmdline\0quiet vx.node=hpc1\0";
        assert_eq!(guest_node(ch).as_deref(), Some("hpc1"));
        assert_eq!(guest_node(b"/bin/bash\0-c\0grep vx.node=zone-fl\0"), None);
        assert_eq!(guest_node(b"qemu-system-x86_64\0-m\x00128m\0"), None);
    }

    /// This test's own process has the marker on no VMM command line, and the check must not read
    /// any host's ordinary interfaces as a vehicle.
    #[test]
    fn the_scan_runs_on_a_real_host() {
        let _ = foreign_objects(&nodes());
    }
}
