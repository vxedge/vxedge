//! What is running on this host, in a run's own slot, that its workspace does not declare.
//!
//! The daemon's memory and its record of the active workspace both belong to one runtime
//! directory, so a daemon started with another one knows neither. The host itself still shows
//! a running vehicle: its gateway ports, and its guests. Reading those needs no privilege.
//!
//! Several runs share a host, each in its own slot (`ARCHITECTURE.md` §6.0.5), so another
//! slot's ports and guests are another run's and not foreign. An object that carries no slot is
//! no run's, and is.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::plan::NodePlan;
use crate::gateway::ifnames::access_port_instance;
use crate::instance::Instance;

const GUEST_MARKER: &str = "vx.node=";
/// The word beside `vx.node=` that names the guest's slot: `vx.slot=<s>`.
pub(crate) const SLOT_MARKER: &str = "vx.slot=";
const CH_BINARY: &str = "cloud-hypervisor";
const CAPSULE_BINARY: &str = "vxcapsule";
const CH_SOCKET_SUFFIX: &str = ".ch.sock";

/// Host objects in slot `instance`, or in no slot, belonging to no node in `nodes`, named for an
/// error message. Sorted, and empty when the slot shows only this workspace or nothing.
pub fn foreign_objects(instance: Instance, nodes: &[NodePlan]) -> Vec<String> {
    let ports = std::fs::read_dir("/sys/class/net")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok());
    let ch_guests = ch_sockets().filter_map(|socket| {
        let name = socket.file_name()?.to_str()?;
        let runtime_name = name.strip_suffix(CH_SOCKET_SUFFIX)?;
        Some((runtime_name_instance(runtime_name), runtime_name.to_owned()))
    });
    foreign(
        instance,
        ports,
        guests().map(|g| (g.instance, g.node)).chain(ch_guests),
        nodes,
    )
}

/// The slot a runtime name `vx<s>-<node>` carries, if it carries one.
fn runtime_name_instance(name: &str) -> Option<Instance> {
    let mut chars = name.strip_prefix("vx")?.chars();
    match (chars.next(), chars.next()) {
        (Some(c), Some('-')) => Instance::from_char(c),
        _ => None,
    }
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

/// A VMM or capsule runner vxedge started, as its command line names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Guest {
    pub pid: i32,
    /// `None` for a guest whose command line names no slot.
    pub instance: Option<Instance>,
    pub node: String,
}

/// Every VMM on this host booted by vxedge, found through `/proc`, so with no runtime dir to
/// depend on.
pub(super) fn guests() -> impl Iterator<Item = Guest> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let pid = e.file_name().to_str()?.parse().ok()?;
            let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
            let (instance, node) = guest_node(&cmdline)?;
            Some(Guest {
                pid,
                instance,
                node,
            })
        })
}

/// Whether an object in `slot` concerns the run in `instance`: its own slot does, another run's
/// does not, and no slot does, since such an object is no run's.
fn in_view(slot: Option<Instance>, instance: Instance) -> bool {
    slot.is_none_or(|s| s == instance)
}

fn foreign(
    instance: Instance,
    links: impl Iterator<Item = String>,
    guests: impl Iterator<Item = (Option<Instance>, String)>,
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
        .filter(|l| access_port_instance(l).is_some_and(|slot| in_view(slot, instance)))
        .filter(|l| !own_ports.contains(l.as_str()))
        .map(|l| format!("port {l}"));
    let guests = guests
        .filter(|(slot, _)| in_view(*slot, instance))
        .filter(|(_, g)| !own_nodes.contains(g.as_str()))
        .map(|(_, g)| format!("guest {g}"));
    let found: BTreeSet<String> = ports.chain(guests).collect();
    found.into_iter().collect()
}

/// The slot and node a NUL-separated `/proc/<pid>/cmdline` is the VMM of, if it is one. A shell
/// that merely mentions the marker is not: only the kernel command line handed to a VMM counts.
fn guest_node(cmdline: &[u8]) -> Option<(Option<Instance>, String)> {
    let cmdline = String::from_utf8_lossy(cmdline);
    let mut args = cmdline.split('\0');
    let bin = args.next()?;
    let args: Vec<&str> = args.collect();
    if Path::new(bin).file_name()?.to_str()? == CAPSULE_BINARY {
        let after = |flag: &str| args.iter().skip_while(|arg| **arg != flag).nth(1).copied();
        let node = after("--node")?.to_owned();
        return Some((after("--slot").and_then(|s| Instance::parse(s).ok()), node));
    }
    if !bin.contains("qemu") {
        return None;
    }
    let words: Vec<&str> = args.into_iter().flat_map(guest_words).collect();
    let node = words
        .iter()
        .find_map(|word| word.strip_prefix(GUEST_MARKER))?
        .to_owned();
    let instance = words
        .iter()
        .find_map(|word| word.strip_prefix(SLOT_MARKER))
        .and_then(|s| Instance::parse(s).ok());
    Some((instance, node))
}

/// The words of one QEMU argument that may carry the `vx.node=` marker: a kernel command line's
/// space-separated words, and the `arg=` values of a `qemu-mcu` guest's `-semihosting-config`.
pub(super) fn guest_words(arg: &str) -> impl Iterator<Item = &str> {
    arg.split([' ', ','])
        .map(|word| word.strip_prefix("arg=").unwrap_or(word))
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
        let gateway = GatewayPlan::from_model(&model, crate::instance::Instance::FIRST).unwrap();
        crate::zones::plan_nodes_from_model(&model, &gateway, std::path::Path::new(".")).unwrap()
    }

    fn strings(names: &[&str]) -> impl Iterator<Item = String> {
        names
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    const I0: Instance = Instance::FIRST;

    fn slotted(slot: &str, nodes: &[&str]) -> Vec<(Option<Instance>, String)> {
        nodes
            .iter()
            .map(|n| (Instance::parse(slot).ok(), n.to_string()))
            .collect()
    }

    #[test]
    fn a_host_showing_only_this_workspace_has_nothing_foreign() {
        let links = strings(&["lo", "docker0", "vxbr0", "vz0-ecu1", "vp0-ecu1"]);
        assert!(foreign(I0, links, slotted("0", &["ecu1"]).into_iter(), &nodes()).is_empty());
    }

    #[test]
    fn another_vehicles_port_and_guest_in_this_slot_are_named() {
        let links = strings(&["vxbr0", "vz0-ecu1", "vz0-node-a"]);
        assert_eq!(
            foreign(
                I0,
                links,
                slotted("0", &["node-a", "node-a"]).into_iter(),
                &nodes()
            ),
            ["guest node-a", "port vz0-node-a"]
        );
    }

    /// Two runs of one vehicle share the host: the same node in another slot is the other run's.
    #[test]
    fn another_slots_ports_and_guests_are_another_runs() {
        let links = strings(&["vxbr1", "vz1-ecu1", "vp1-ecu1", "vz1-node-a"]);
        assert!(
            foreign(
                I0,
                links,
                slotted("1", &["ecu1", "node-a"]).into_iter(),
                &nodes()
            )
            .is_empty()
        );
    }

    /// A name from before slots, or a guest with no slot, is no run's: still foreign to every slot.
    #[test]
    fn an_object_with_no_slot_is_foreign() {
        let links = strings(&["vz-ecu1"]);
        let guests = vec![(None, "ecu1".to_string())];
        assert_eq!(
            foreign(I0, links, guests.into_iter(), &[]),
            ["guest ecu1", "port vz-ecu1"]
        );
    }

    #[test]
    fn only_a_vmm_counts_as_a_guest() {
        let qemu = b"qemu-system-x86_64\0-append\0console=ttyS0 vx.node=zone-fl vx.slot=0\0";
        assert_eq!(guest_node(qemu), Some((Some(I0), "zone-fl".into())));
        let unslotted = b"qemu-system-x86_64\0-append\0console=ttyS0 vx.node=zone-fl\0";
        assert_eq!(guest_node(unslotted), Some((None, "zone-fl".into())));
        assert_eq!(guest_node(b"/bin/bash\0-c\0grep vx.node=zone-fl\0"), None);
        assert_eq!(guest_node(b"qemu-system-x86_64\0-m\x00128m\0"), None);
        let mcu = b"qemu-system-arm\0-semihosting-config\0enable=on,target=native,arg=vx.ip=10.0.0.5/24,arg=vx.node=ecu-m3,arg=vx.slot=0\0";
        assert_eq!(guest_node(mcu), Some((Some(I0), "ecu-m3".into())));
    }

    #[test]
    fn a_capsule_runner_counts_as_a_guest_of_its_node() {
        let runner = b"/opt/vxedge/vxcapsule\0--slot\x000\0--node\0sensor-01\0/parts/s.wasm\0";
        assert_eq!(guest_node(runner), Some((Some(I0), "sensor-01".into())));
        assert_eq!(guest_node(b"/opt/vxedge/vxcapsule\0--version\0"), None);
        let shell = b"/bin/bash\0-c\0vxcapsule --slot 0 --node sensor-01 s.wasm\0";
        assert_eq!(guest_node(shell), None);
    }

    #[test]
    fn a_runtime_name_says_its_slot() {
        assert_eq!(runtime_name_instance("vx0-hpc1"), Some(I0));
        assert_eq!(runtime_name_instance("vx-hpc1"), None);
        assert_eq!(runtime_name_instance("vxgptp-vxbr0"), None);
    }

    #[test]
    fn a_cloud_hypervisor_vmm_is_found_by_its_api_socket() {
        let vmm = b"cloud-hypervisor\0--api-socket\0path=/run/a/vx0-hpc1.ch.sock\0";
        assert_eq!(
            ch_socket(vmm),
            Some(PathBuf::from("/run/a/vx0-hpc1.ch.sock"))
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
        assert!(
            foreign(
                I0,
                std::iter::empty(),
                [(Some(I0), own)].into_iter(),
                &nodes()
            )
            .is_empty()
        );
    }

    /// This test's own process has the marker on no VMM command line, and the check must not read
    /// any host's ordinary interfaces as a vehicle.
    #[test]
    fn the_scan_runs_on_a_real_host() {
        let _ = foreign_objects(I0, &nodes());
    }
}
