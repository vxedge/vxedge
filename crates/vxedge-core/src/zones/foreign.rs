//! What is running on this host that a workspace does not declare.
//!
//! The daemon's memory and its record of the active workspace both belong to one runtime
//! directory, so a daemon started with another one knows neither. The host itself still shows
//! a running vehicle: its gateway ports, and its guests. Reading those needs no privilege.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::plan::NodePlan;

const ACCESS_PREFIX: &str = "vz-";
const GUEST_MARKER: &str = "vx.node=";
const CH_BINARY: &str = "cloud-hypervisor";
const CH_SOCKET_SUFFIX: &str = ".ch.sock";

/// Host objects belonging to no node in `nodes`, named for an error message. Sorted, and empty
/// when the host shows only this workspace or nothing.
pub fn foreign_objects(nodes: &[NodePlan]) -> Vec<String> {
    let ports = std::fs::read_dir("/sys/class/net")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok());
    let ch_guests = ch_sockets().filter_map(|socket| {
        let name = socket.file_name()?.to_str()?;
        Some(name.strip_suffix(CH_SOCKET_SUFFIX)?.to_owned())
    });
    foreign(
        ports,
        guests().map(|(_, node)| node).chain(ch_guests),
        nodes,
    )
}

/// The name a node's Cloud Hypervisor API socket has in any runtime dir.
pub(super) fn ch_socket_name(np: &NodePlan) -> String {
    format!("{}{CH_SOCKET_SUFFIX}", np.runtime_name())
}

/// The API socket of every Cloud Hypervisor VMM on this host, found through `/proc`.
///
/// A guest's kernel command line reaches the VMM over the API, so [`guests`] cannot see one. The
/// socket is what its command line does carry, and its file name identifies the node.
pub(super) fn ch_sockets() -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name().to_str()?.parse::<i32>().ok()?;
            ch_socket(&std::fs::read(e.path().join("cmdline")).ok()?)
        })
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
    let runtime_names: Vec<String> = nodes.iter().map(NodePlan::runtime_name).collect();
    let own_nodes: BTreeSet<&str> = nodes
        .iter()
        .map(|n| n.node_id.as_str())
        .chain(runtime_names.iter().map(String::as_str))
        .collect();
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
    if !bin.contains("qemu") {
        return None;
    }
    args.flat_map(|arg| arg.split(' '))
        .find_map(|word| word.strip_prefix(GUEST_MARKER))
        .map(str::to_owned)
}

/// The API socket a NUL-separated `/proc/<pid>/cmdline` names, if it is a Cloud Hypervisor VMM
/// vxedge started.
fn ch_socket(cmdline: &[u8]) -> Option<PathBuf> {
    let cmdline = String::from_utf8_lossy(cmdline);
    let mut args = cmdline.split('\0');
    if Path::new(args.next()?).file_name()?.to_str()? != CH_BINARY {
        return None;
    }
    let socket = args.skip_while(|arg| *arg != "--api-socket").nth(1)?;
    let path = PathBuf::from(socket.strip_prefix("path=")?);
    path.file_name()?
        .to_str()?
        .ends_with(CH_SOCKET_SUFFIX)
        .then_some(path)
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
        assert_eq!(guest_node(b"/bin/bash\0-c\0grep vx.node=zone-fl\0"), None);
        assert_eq!(guest_node(b"qemu-system-x86_64\0-m\x00128m\0"), None);
    }

    #[test]
    fn a_cloud_hypervisor_vmm_is_found_by_its_api_socket() {
        let vmm = b"cloud-hypervisor\0--api-socket\0path=/run/a/vx-hpc1.ch.sock\0";
        assert_eq!(
            ch_socket(vmm),
            Some(PathBuf::from("/run/a/vx-hpc1.ch.sock"))
        );
        let full_path = b"/usr/local/bin/cloud-hypervisor\0--api-socket\0path=/r/n.ch.sock\0";
        assert_eq!(ch_socket(full_path), Some(PathBuf::from("/r/n.ch.sock")));
    }

    #[test]
    fn only_a_vxedge_vmm_counts_as_a_cloud_hypervisor_guest() {
        let shell = b"/bin/bash\0-c\0cloud-hypervisor --api-socket path=/r/n.ch.sock\0";
        assert_eq!(ch_socket(shell), None);
        assert_eq!(
            ch_socket(b"cloud-hypervisor\0--api-socket\0path=/run/other.sock\0"),
            None
        );
        assert_eq!(ch_socket(b"cloud-hypervisor\0--api-socket\0fd=3\0"), None);
        assert_eq!(ch_socket(b"cloud-hypervisor\0--version\0"), None);
    }

    #[test]
    fn this_workspaces_own_cloud_hypervisor_guest_is_not_foreign() {
        let own = nodes()[0].runtime_name();
        assert!(foreign(std::iter::empty(), [own].into_iter(), &nodes()).is_empty());
    }

    /// This test's own process has the marker on no VMM command line, and the check must not read
    /// any host's ordinary interfaces as a vehicle.
    #[test]
    fn the_scan_runs_on_a_real_host() {
        let _ = foreign_objects(&nodes());
    }
}
