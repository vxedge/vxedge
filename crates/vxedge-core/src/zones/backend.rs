//! Node lifecycle per packaging backend. All three attach the node to the
//! gateway port the same way; only the isolation mechanism differs — that is
//! exactly the H3 variable.
//!
//! - `container`: a network namespace; the veth peer moves in, renamed eth0,
//!   addressed from the manifest. With no planned image (`local/…` parts) it is
//!   a named netns and the experiment runner execs the payload into it. With a
//!   planned OCI image (`NodePlan::image`) the namespace belongs to a container
//!   group's infra container (loopback only) and the image runs as its member.
//! - `qemu-microvm`: raw QEMU `microvm` machine, direct kernel boot from the
//!   node's image bundle (vmlinuz + initrd.img), virtio-net on the tap.
//!   Guest init reads `vx.ip=` from the kernel cmdline; payload autostarts.
//! - `wasm-capsule`: a `vxcapsule` runner in a network namespace, see
//!   [`super::wasm_capsule`].
//! - `libvirt`: full QEMU machine via virsh (qemu:///system), same bundle and
//!   cmdline, precreated tap (`managed='no'`).
//!
//! Ordering contract: `gateway up` before `zone up`; `zone down` before
//! `gateway down`. Deleting a container netns destroys its veth pair, so
//! re-running `vxedge up` (idempotent) restores endpoints after `zone down`.
//!
//! All three are selected through one [`ZoneBackend`] seam, and [`backend_for`] is the only
//! place a backend kind is matched. Adding a fourth is an impl and one arm — see the trait's
//! doc for what an implementation owes.

use std::path::Path;

/// The emulator an x86_64 `qemu-microvm` node runs. `HostService.Capabilities` probes this same
/// name, so it cannot report a backend the backend itself would not start for an x86_64 guest.
/// An aarch64 guest runs [`GuestArch::Aarch64`]'s emulator, which `up` checks for itself.
pub const QEMU_MICROVM_BIN: &str = "qemu-system-x86_64";

/// The instruction set of a VM guest bundle, from its `meta.json`: `arch`, or the
/// `org.vxedge.arch` a published vxparts bundle carries (ARCHITECTURE.md §9). A bundle that names
/// none is x86_64, as every bundle was before the key existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestArch {
    X86_64,
    Aarch64,
    /// A Cortex-M microcontroller, which only `qemu-mcu` runs, and only under TCG.
    Armv7m,
}

impl GuestArch {
    /// The name `meta.json` and `NodeSpec.arch` use.
    pub fn as_str(self) -> &'static str {
        match self {
            GuestArch::X86_64 => "x86_64",
            GuestArch::Aarch64 => "aarch64",
            GuestArch::Armv7m => "armv7m",
        }
    }

    /// The names `qemu-microvm` boots: an MCU's `armv7m` is not one of them.
    fn from_name(name: &str) -> Option<Self> {
        match name {
            // A 32-bit x86 guest (the Zephyr probe is `i386`) runs on the same x86_64 machine.
            "x86_64" | "i386" | "i686" | "x86" => Some(GuestArch::X86_64),
            "aarch64" | "arm64" => Some(GuestArch::Aarch64),
            _ => None,
        }
    }

    /// The QEMU system emulator for this instruction set.
    pub fn qemu_bin(self) -> &'static str {
        match self {
            GuestArch::X86_64 => QEMU_MICROVM_BIN,
            GuestArch::Aarch64 => "qemu-system-aarch64",
            GuestArch::Armv7m => super::qemu_mcu::QEMU_MCU_BIN,
        }
    }

    /// The host's own instruction set, the only one KVM can run.
    pub fn host() -> Option<Self> {
        Self::from_name(std::env::consts::ARCH)
    }
}

/// How a VM guest's CPU runs: KVM on a host of the same instruction set, otherwise QEMU's
/// emulator (TCG). A `tcg` run is functional evidence only and is never timed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceleration {
    Kvm,
    Tcg,
}

impl Acceleration {
    pub fn as_str(self) -> &'static str {
        match self {
            Acceleration::Kvm => "kvm",
            Acceleration::Tcg => "tcg",
        }
    }

    fn for_guest(arch: GuestArch, kvm_device: bool) -> Self {
        if kvm_device && GuestArch::host() == Some(arch) {
            Acceleration::Kvm
        } else {
            Acceleration::Tcg
        }
    }
}

/// What a running VM guest runs on, read from its own QEMU command line, so it reports what
/// actually started and not what the bundle asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestPlatform {
    pub arch: GuestArch,
    pub acceleration: Acceleration,
}

/// Whether `bin` is an executable file on `PATH`.
fn on_path(bin: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(bin))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

/// What a VM guest bundle's `meta.json` says about how it boots. The file is optional, and every
/// key has the default every bundle had before it existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BundleBoot {
    /// `arch`, or the `org.vxedge.arch` a published vxparts bundle carries. Default x86_64.
    arch: GuestArch,
    /// `"virtio_mmio": "legacy"`: the guest's driver speaks only the version 1 (legacy) virtio-mmio
    /// transport. Default `modern`, the version 2 transport, which
    /// Zephyr's driver requires.
    legacy_mmio: bool,
    /// `cpus`: vCPUs. Default 1. With pinned CPUs, at most as many as are pinned.
    cpus: u32,
    /// `memory_mb`: guest memory. Default 128 MiB on x86_64, 256 MiB on aarch64.
    memory_mb: Option<u32>,
    /// `"virtualization": true`: the guest is itself a hypervisor and needs the CPU's
    /// virtualization extensions. aarch64 only: EL2 on `virt`, with GICv3.
    virtualization: bool,
    /// `"org.vxedge.virtio-pci": "true"`: the guest's kernel drives a virtio-net on a PCI bus, so
    /// a device may ask for `qemu.transport: pci`. Default false: the FreeRTOS and Zephyr probe
    /// guests drive virtio-mmio only, and on PCI they would boot with no NIC.
    virtio_pci: bool,
}

/// The ranges `cpus` and `memory_mb` may take: a zone ECU guest, not a server.
const MAX_CPUS: u32 = 16;
const MEMORY_MB: std::ops::RangeInclusive<u32> = 64..=65536;

fn bundle_boot(np: &NodePlan) -> Result<BundleBoot, ZoneBackendError> {
    let default = BundleBoot {
        arch: GuestArch::X86_64,
        legacy_mmio: false,
        cpus: 1,
        memory_mb: None,
        virtualization: false,
        virtio_pci: false,
    };
    let Some(dir) = np.bundle.as_deref() else {
        return Ok(default);
    };
    let Ok(text) = std::fs::read_to_string(dir.join("meta.json")) else {
        return Ok(default);
    };
    let refuse = |detail: String| ZoneBackendError::Runtime {
        node: np.node_id.clone(),
        detail,
    };
    let meta: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| refuse(format!("the bundle's meta.json is not JSON: {e}")))?;
    let named = ["arch", "org.vxedge.arch"]
        .iter()
        .find_map(|key| meta.get(*key).and_then(|a| a.as_str()));
    let arch = match named {
        None => GuestArch::X86_64,
        Some(name) => GuestArch::from_name(name).ok_or_else(|| {
            refuse(format!(
                "the bundle's meta.json names arch `{name}`; qemu-microvm boots x86 (x86_64, i386) and \
                 aarch64 (microcontroller firmware runs on `backend: qemu-mcu`)"
            ))
        })?,
    };
    let legacy_mmio = match meta.get("virtio_mmio").and_then(|v| v.as_str()) {
        None | Some("modern") => false,
        Some("legacy") => true,
        Some(other) => {
            return Err(refuse(format!(
                "the bundle's meta.json names virtio_mmio `{other}`; it is `legacy` or `modern`"
            )));
        }
    };
    let number = |key: &str| -> Result<Option<u32>, ZoneBackendError> {
        match meta.get(key) {
            None => Ok(None),
            Some(v) => v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| {
                    refuse(format!(
                        "the bundle's meta.json `{key}` is not a whole number: {v}"
                    ))
                }),
        }
    };
    let cpus = number("cpus")?.unwrap_or(1);
    if !(1..=MAX_CPUS).contains(&cpus) {
        return Err(refuse(format!(
            "the bundle's meta.json asks for {cpus} cpus; 1 to {MAX_CPUS}"
        )));
    }
    if !np.pinned_cpus.is_empty() && cpus as usize > np.pinned_cpus.len() {
        return Err(refuse(format!(
            "the bundle asks for {cpus} cpus, but the node is pinned to {}",
            np.pinned_cpus.len()
        )));
    }
    let memory_mb = number("memory_mb")?;
    if let Some(mb) = memory_mb.filter(|mb| !MEMORY_MB.contains(mb)) {
        return Err(refuse(format!(
            "the bundle's meta.json asks for {mb} MiB; {} to {} MiB",
            MEMORY_MB.start(),
            MEMORY_MB.end()
        )));
    }
    let virtualization = match meta.get("virtualization") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| {
            refuse(format!(
                "the bundle's meta.json `virtualization` is not true or false: {v}"
            ))
        })?,
    };
    if virtualization && arch != GuestArch::Aarch64 {
        return Err(refuse(
            "the bundle asks for `virtualization`, which qemu-microvm offers on aarch64 only"
                .into(),
        ));
    }
    let virtio_pci = match meta.get("org.vxedge.virtio-pci") {
        None => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) if s == "true" || s == "false" => s == "true",
        Some(v) => {
            return Err(refuse(format!(
                "the bundle's meta.json `org.vxedge.virtio-pci` is not true or false: {v}"
            )));
        }
    };
    Ok(BundleBoot {
        arch,
        legacy_mmio,
        cpus,
        memory_mb,
        virtualization,
        virtio_pci,
    })
}

use super::cloud_hypervisor;
use super::plan::NodePlan;
use super::wasm_capsule;
use crate::containers::{
    ContainerClient, ContainerError, ContainerSpec, GroupHandle, GroupSpec, InfraNetwork,
    PAUSE_IMAGE,
};
use crate::gateway::netlink;
use crate::gateway::netops::children_are_privileged;
use crate::gateway::netops::{NetOpsError, Shell};
use crate::model::Backend;
use crate::model::{QemuMachine, QemuOptions, VirtioTransport};

#[derive(Debug, thiserror::Error)]
pub enum ZoneBackendError {
    #[error(transparent)]
    NetOps(#[from] NetOpsError),
    #[error(
        "node `{node}`: bundle file missing: {path} (a local bundle is built per host, since it \
         embeds the host kernel: run the part's `build.sh <bundle-dir>`, see its README)"
    )]
    BundleFile { node: String, path: String },
    #[error("node `{node}`: {detail}")]
    Runtime { node: String, detail: String },
    #[error(
        "node `{node}`: gateway port `{port}` is gone (it is destroyed with the namespace it was \
         moved into). Re-run `vxedge up` to recreate it, then bring the zone up again"
    )]
    PortGone { node: String, port: String },
    #[error("node `{node}`: fetching published bundle `{reference}` failed: {source}")]
    Fetch {
        node: String,
        reference: String,
        #[source]
        source: crate::registry::RegistryFetchError,
    },
    #[error("node `{node}`: container payload: {source}")]
    Containers {
        node: String,
        #[source]
        source: Box<ContainerError>,
    },
}

/// Wraps a [`netlink::NetlinkError`] with the node this seam's callers actually need to act on
/// — mirrors `gateway::provision`'s own `nl()` mapper, adapted to carry the node id
/// `ZoneBackendError::Runtime` already asks for rather than a bare command line.
fn nl(np: &NodePlan, e: netlink::NetlinkError) -> ZoneBackendError {
    ZoneBackendError::Runtime {
        node: np.node_id.clone(),
        detail: format!("netlink: {e}"),
    }
}

/// Observed state of one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Up,
    Down,
}

impl crate::status::Observable for NodeState {
    fn state(&self) -> crate::status::ObservedState {
        match self {
            NodeState::Up => crate::status::ObservedState::Up,
            NodeState::Down => crate::status::ObservedState::Down,
        }
    }
}

pub(super) fn kernel_cmdline(np: &NodePlan) -> String {
    kernel_cmdline_for(np, GuestArch::X86_64)
}

/// The guest's console is the machine's first serial port: the 8250 `ttyS0` on x86_64, the
/// PL011 `ttyAMA0` on an aarch64 `virt` machine.
fn kernel_cmdline_for(np: &NodePlan, arch: GuestArch) -> String {
    let ip = np.ip_cidr.as_deref().unwrap_or("");
    let console = match arch {
        GuestArch::X86_64 => "ttyS0",
        GuestArch::Aarch64 | GuestArch::Armv7m => "ttyAMA0",
    };
    format!(
        "console={console} reboot=t panic=-1 quiet vx.ip={ip} vx.node={}",
        np.node_id
    )
}

pub(super) fn bundle_paths(np: &NodePlan) -> Result<(String, String), ZoneBackendError> {
    let dir = np
        .bundle
        .as_deref()
        .expect("plan invariant: vm backends carry a bundle");
    let kernel = dir.join("vmlinuz");
    let initrd = dir.join("initrd.img");
    for p in [&kernel, &initrd] {
        if !p.exists() {
            return Err(ZoneBackendError::BundleFile {
                node: np.node_id.clone(),
                path: p.display().to_string(),
            });
        }
    }
    Ok((kernel.display().to_string(), initrd.display().to_string()))
}

// ---------------------------------------------------------------------------
// container (netns)
// ---------------------------------------------------------------------------

/// A named namespace for the node, holding its gateway port as `eth0`.
pub(super) fn netns_port_up(sh: &Shell, np: &NodePlan) -> Result<(), ZoneBackendError> {
    let ns = np.runtime_name();
    // Netns registry ops run in the HOST context — never nested inside another netns, since
    // a bind mount created from inside one doesn't propagate; only the veth move targets
    // where the interface currently lives. See `gateway::netlink`'s own module doc for why
    // netns create/delete is `rtnetlink::NetworkNamespace`, not a hand-rolled unshare/mount.
    if escalates_netns_registry(sh) {
        if !netlink::netns_exists(&ns) {
            sh.run_host("ip", &["netns", "add", &ns])?;
        }
    } else {
        netlink::ensure_netns(&ns).map_err(|e| nl(np, e))?;
    }
    attach_port(sh, np, &ns)
}

/// Whether a named namespace has to go through `ip` and sudo. `/run/netns` is root's, and
/// `cap_net_admin,cap_sys_admin` do not reach a directory another user owns, so a `vxedged` under
/// `setcap` cannot register a namespace itself. Everything after that works with its own
/// capabilities.
fn escalates_netns_registry(sh: &Shell) -> bool {
    sh.escalate && !children_are_privileged()
}

fn container_up(sh: &Shell, np: &NodePlan) -> Result<(), ZoneBackendError> {
    let Some(image) = &np.image else {
        return netns_port_up(sh, np);
    };
    // Order matters: the namespace exists (infra up), then the port is in it, then the payload
    // starts — a payload started first would come up with nothing but loopback to bind to.
    let group = payload_group(np)?;
    let client = block_on(ContainerClient::connect_user()).map_err(|e| ct(np, e))?;
    let (handle, _) = block_on(client.ensure_group(&group)).map_err(|e| ct(np, e))?;
    attach_port(sh, np, &infra_netns(&handle))?;
    block_on(client.ensure_member(&group, &handle, &payload_spec(image, &np.pinned_cpus)))
        .map_err(|e| ct(np, e))?;
    Ok(())
}

/// Moves the node's gateway port into `ns` (a registry name or an absolute namespace path) as
/// `eth0`, addresses it and brings it up. Idempotent: a port already moved is not looked for on
/// the host again.
fn attach_port(sh: &Shell, np: &NodePlan, ns: &str) -> Result<(), ZoneBackendError> {
    if netlink::link_exists(sh.netns.as_deref(), &np.peer) {
        netlink::move_to_netns(sh.netns.as_deref(), &np.peer, ns).map_err(|e| nl(np, e))?;
        netlink::rename_link(Some(ns), &np.peer, "eth0").map_err(|e| nl(np, e))?;
    } else if !netlink::link_exists(Some(ns), "eth0") {
        // Neither on the host nor in the namespace: the pair died with an earlier namespace
        // (`zone down`, or an infra container that was replaced), and only the gateway makes
        // ports.
        return Err(ZoneBackendError::PortGone {
            node: np.node_id.clone(),
            port: np.peer.clone(),
        });
    }
    netlink::link_up(Some(ns), "lo").map_err(|e| nl(np, e))?;
    for cidr in np.ip_cidr.iter().chain(&np.extra_cidrs) {
        netlink::add_address(Some(ns), "eth0", cidr).map_err(|e| nl(np, e))?;
    }
    netlink::link_up(Some(ns), "eth0").map_err(|e| nl(np, e))?;
    Ok(())
}

pub(super) fn container_down(sh: &Shell, np: &NodePlan) -> Result<(), ZoneBackendError> {
    // Asked before anything is removed: a port still on the gateway side (a node never brought
    // up) has no namespace to die with, so waiting for it would only time out.
    let port_moved = port_netns(np).is_some_and(|ns| netlink::link_exists(Some(&ns), "eth0"));
    // Destroys the moved veth peer (and thus the bridge-side pair);
    // `vxedge up` re-creates endpoints idempotently.
    let ns = np.runtime_name();
    if !escalates_netns_registry(sh) {
        netlink::delete_netns(&ns).map_err(|e| nl(np, e))?;
    } else if netlink::netns_exists(&ns) {
        sh.run_host("ip", &["netns", "del", &ns])?;
    }
    if np.image.is_some() {
        // The group's namespace, and the port in it, go with its infra container.
        let client = block_on(ContainerClient::connect_user()).map_err(|e| ct(np, e))?;
        block_on(client.group_down(&np.runtime_name())).map_err(|e| ct(np, e))?;
    }
    // The kernel tears a namespace down asynchronously, so the host side of the pair outlives
    // the delete, usually by some milliseconds. Returning early lets an `up` straight after find the dying port, count it as already
    // there, and be left with none (`gateway_netns` failed this way on 2026-10-31).
    if port_moved
        && !wait_until(PORT_GONE_TIMEOUT, || {
            !netlink::link_exists(sh.netns.as_deref(), &np.endpoint)
        })
    {
        return Err(ZoneBackendError::Runtime {
            node: np.node_id.clone(),
            detail: format!(
                "gateway port `{}` still exists {} s after its namespace was removed. Something \
                 may be holding the namespace open (`ip netns identify`, or a process still \
                 inside it): stop it and re-run `zone down`",
                np.endpoint,
                PORT_GONE_TIMEOUT.as_secs()
            ),
        });
    }
    Ok(())
}

/// How long `down` waits for the kernel to finish destroying a port.
const PORT_GONE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Polls `done` until it holds or `timeout` passes; returns whether it held.
pub(super) fn wait_until(timeout: std::time::Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The namespace currently holding this node's port, if it has one: the registry name, or the
/// group's infra namespace. Any daemon error reads as none.
fn port_netns(np: &NodePlan) -> Option<String> {
    match &np.image {
        None => {
            let ns = np.runtime_name();
            netlink::netns_exists(&ns).then_some(ns)
        }
        Some(_) => block_on(async {
            let client = ContainerClient::connect_user().await.ok()?;
            client.handle(&np.runtime_name()).await.ok()
        })
        .map(|handle| infra_netns(&handle)),
    }
}

/// Run `spec` as one more member of a running node's container group, in its namespace: a
/// service placed on the device (`ARCHITECTURE.md` §6.0.1). Refuses a node that is not up,
/// because a group brought up here would have no gateway port. Idempotent.
pub fn member_up(
    sh: &Shell,
    np: &NodePlan,
    runtime_dir: &Path,
    spec: &ContainerSpec,
) -> Result<(), ZoneBackendError> {
    if node_status(sh, np, runtime_dir) != NodeState::Up {
        return Err(ZoneBackendError::Runtime {
            node: np.node_id.clone(),
            detail: format!(
                "`{}` cannot join a node that is not up. Bring the node up first",
                spec.name
            ),
        });
    }
    let group = payload_group(np)?;
    let client = block_on(ContainerClient::connect_user()).map_err(|e| ct(np, e))?;
    let (handle, _) = block_on(client.ensure_group(&group)).map_err(|e| ct(np, e))?;
    block_on(client.ensure_member(&group, &handle, spec)).map_err(|e| ct(np, e))?;
    Ok(())
}

/// Stop one member of a node's group and leave the node running. Idempotent.
pub fn member_down(np: &NodePlan, spec: &ContainerSpec) -> Result<(), ZoneBackendError> {
    let group = payload_group(np)?;
    let client = block_on(ContainerClient::connect_user()).map_err(|e| ct(np, e))?;
    block_on(client.member_down(&group, spec)).map_err(|e| ct(np, e))
}

/// Whether that member is running, as Podman reports it. An unreachable Podman reads as down.
pub fn member_status(np: &NodePlan, spec: &ContainerSpec) -> NodeState {
    let running = payload_group(np).ok().and_then(|group| {
        let name = group.member_container_name(spec).ok()?;
        block_on(async {
            let client = ContainerClient::connect_user().await.ok()?;
            let status = client.group_status(group.name()).await.ok()?;
            Some(status.member(&name).is_some_and(|c| c.is_running()))
        })
    });
    if running == Some(true) {
        NodeState::Up
    } else {
        NodeState::Down
    }
}

fn container_status(_sh: &Shell, np: &NodePlan) -> NodeState {
    let ns = match &np.image {
        None => {
            let ns = np.runtime_name();
            if !netlink::netns_exists(&ns) {
                return NodeState::Down;
            }
            ns
        }
        Some(_) => match payload_handle(np) {
            Some(handle) => infra_netns(&handle),
            None => return NodeState::Down,
        },
    };
    if netlink::link_exists(Some(&ns), "eth0") {
        NodeState::Up
    } else {
        NodeState::Down
    }
}

/// The running group's handle when infra and payload both run; any daemon error reads as down,
/// since `status` reports and never fails.
fn payload_handle(np: &NodePlan) -> Option<GroupHandle> {
    block_on(async {
        let client = ContainerClient::connect_user().await.ok()?;
        let status = client.group_status(&np.runtime_name()).await.ok()?;
        if !status.all_running() {
            return None;
        }
        client.handle(&np.runtime_name()).await.ok()
    })
}

/// The group a container node's payload runs in, named after the node's runtime name. Its infra
/// container is on loopback only ([`InfraNetwork::None`]): the gateway port is the node's one
/// interface, as in the named-netns form and as in the VM arms.
fn payload_group(np: &NodePlan) -> Result<GroupSpec, ZoneBackendError> {
    GroupSpec::new(
        np.runtime_name(),
        ContainerSpec {
            image: PAUSE_IMAGE.to_string(),
            cpuset_cpus: np.pinned_cpus.clone(),
            ..Default::default()
        },
    )
    .map(|g| g.with_infra_network(InfraNetwork::None))
    .map_err(|e| ct(np, e.into()))
}

/// Seconds a zone payload gets to exit after SIGTERM before Podman kills it. A payload that is
/// PID 1 and installs no handler ignores SIGTERM, so Podman's default 10 s was spent in full on
/// every such container at `down`.
const PAYLOAD_STOP_TIMEOUT_S: u32 = 2;

/// The payload: the planned image with its own default command, on the device's pinned CPUs if
/// it has any. Nothing else is set, so what runs is exactly what was published under that digest.
fn payload_spec(image: &str, pinned_cpus: &[u32]) -> ContainerSpec {
    ContainerSpec {
        name: "payload".to_string(),
        image: image.to_string(),
        cpuset_cpus: pinned_cpus.to_vec(),
        stop_timeout_s: Some(PAYLOAD_STOP_TIMEOUT_S),
        ..Default::default()
    }
}

fn infra_netns(handle: &GroupHandle) -> String {
    format!("/proc/{}/ns/net", handle.infra_pid)
}

fn ct(np: &NodePlan, e: ContainerError) -> ZoneBackendError {
    ZoneBackendError::Containers {
        node: np.node_id.clone(),
        source: Box::new(e),
    }
}

/// Drives a container-client future from this synchronous seam. Every caller already runs the
/// seam off the async runtime (`vxedged`'s `blocking(...)`/`spawn_blocking`), so the future runs
/// on the caller's runtime through [`tokio::runtime::Handle::block_on`]; that call panics on a
/// runtime worker thread, which is why the seam must never be called from one directly. With no
/// runtime at all (a plain `#[test]`), a current-thread runtime is built for the call.
///
/// A client is connected per operation rather than held: the trait carries no state, a connect
/// is one version request on a local socket, and a cached client would stay bound to whichever
/// runtime first used it.
pub(super) fn block_on<F: std::future::Future>(f: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(f),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a current-thread tokio runtime for a container call")
            .block_on(f),
    }
}

// ---------------------------------------------------------------------------
// qemu-microvm (raw QEMU, pidfile-managed)
// ---------------------------------------------------------------------------

fn microvm_up(sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
    if microvm_status(np, runtime_dir) == NodeState::Up {
        return Ok(()); // idempotent
    }
    let (kernel, initrd) = bundle_paths(np)?;
    let boot = bundle_boot(np)?;
    check_qemu_options(np, boot, vhost_net_usable)?;
    let arch = boot.arch;
    let accel = Acceleration::for_guest(arch, Path::new("/dev/kvm").exists());
    if !on_path(arch.qemu_bin()) {
        return Err(ZoneBackendError::Runtime {
            node: np.node_id.clone(),
            detail: format!(
                "an {} guest needs `{}` on PATH",
                arch.as_str(),
                arch.qemu_bin()
            ),
        });
    }
    let args = microvm_argv(np, runtime_dir, &kernel, &initrd, boot, accel);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    // Deliberately not `sh.run()` unchanged: qemu itself needs no privilege beyond `/dev/kvm`
    // (granted via the `kvm` group, not root) and an already-owned tap
    // (`tuntap::create_and_persist` sets `TUNSETOWNER` to this same process's uid) — running it
    // under `sudo` only reintroduces the problem that ownership fix solves, a root-owned
    // pidfile/serial log this same unprivileged process can no longer read back afterward
    // (`read_pid`, `microvm_status`, `microvm_down`'s own `kill`). Live-found 2026-08-06: `zone
    // up` reported "verification failed — nodes not up" for a node whose qemu process was
    // genuinely running, because `read_pid`'s plain `std::fs::read_to_string` hit EACCES on a
    // pidfile `sh.run`'s unconditional `sudo` had written as root. Escalation is still needed to
    // *enter* a confined netns (`ip netns exec` requires `CAP_SYS_ADMIN`), so keep it only then.
    let qemu_sh = Shell {
        netns: sh.netns.clone(),
        escalate: sh.escalate && sh.netns.is_some(),
    };
    qemu_sh.run(argv[0], &argv[1..])?;
    Ok(())
}

/// Whether this process can open `/dev/vhost-net` for reading and writing, as QEMU's
/// `vhost=on` does.
fn vhost_net_usable() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/vhost-net")
        .is_ok()
}

/// The checks on a device's `qemu:` block that depend on its bundle or on this host, so the
/// manifest alone cannot settle them (`model::build` refuses the rest at plan time).
fn check_qemu_options(
    np: &NodePlan,
    boot: BundleBoot,
    vhost_usable: fn() -> bool,
) -> Result<(), ZoneBackendError> {
    let refuse = |detail: String| {
        Err(ZoneBackendError::Runtime {
            node: np.node_id.clone(),
            detail,
        })
    };
    let q = np.qemu;
    if q != QemuOptions::default() && boot.arch != GuestArch::X86_64 {
        return refuse(format!(
            "`qemu:` options are for an x86 guest; this bundle is {}",
            boot.arch.as_str()
        ));
    }
    if q.transport == VirtioTransport::Pci && !boot.virtio_pci {
        return refuse(
            "`qemu.transport: pci` needs a bundle that declares `org.vxedge.virtio-pci: true`; \
             this one does not, and its guest would boot with no NIC"
                .into(),
        );
    }
    if q.vhost && !vhost_usable() {
        return refuse(
            "`qemu.vhost: true` needs `/dev/vhost-net` open for reading and writing (the `kvm` \
             group on most hosts)"
                .into(),
        );
    }
    Ok(())
}

/// The QEMU command line of a `qemu-microvm` node: a minimal virtio-mmio machine with direct
/// kernel boot, `microvm` on x86_64 and `virt` on aarch64. Pure, so both are tested.
fn microvm_argv(
    np: &NodePlan,
    runtime_dir: &Path,
    kernel: &str,
    initrd: &str,
    boot: BundleBoot,
    accel: Acceleration,
) -> Vec<String> {
    let arch = boot.arch;
    let q = np.qemu;
    let pci = q.transport == VirtioTransport::Pci;
    let transport = format!("virtio-mmio.force-legacy={}", boot.legacy_mmio);
    let name = np.runtime_name();
    let pidfile = runtime_dir.join(format!("{name}.pid"));
    let serial = format!(
        "file:{}",
        runtime_dir.join(format!("{name}.serial.log")).display()
    );
    let vhost = if q.vhost { ",vhost=on" } else { "" };
    let netdev = format!(
        "tap,id=n0,ifname={},script=no,downscript=no{vhost}",
        np.endpoint
    );
    let nic_model = if pci {
        "virtio-net-pci"
    } else {
        "virtio-net-device"
    };
    let nic = format!("{nic_model},netdev=n0,mac={}", np.guest_mac());
    let append = kernel_cmdline_for(np, arch);
    let kvm = accel == Acceleration::Kvm;
    let (machine, cpu, default_mb) = match arch {
        // A PCIe bus on `microvm` needs ACPI: without it the NIC behind the root port stays in
        // D3cold. `q35` always has both.
        GuestArch::X86_64 => (
            match (q.machine, pci) {
                (QemuMachine::Q35, _) => "q35",
                (QemuMachine::Microvm, true) => "microvm,pcie=on,acpi=on",
                (QemuMachine::Microvm, false) => "microvm,acpi=off",
            },
            if kvm { "host" } else { "max" },
            128,
        ),
        // A guest that is itself a hypervisor runs at EL2, and its own guests need the GICv3
        // virtualization interface.
        GuestArch::Aarch64 if boot.virtualization => (
            "virt,virtualization=on,gic-version=3",
            if kvm { "host" } else { "cortex-a57" },
            256,
        ),
        // A distribution arm64 kernel unpacks to about 70 MB, so the guest gets more room.
        GuestArch::Aarch64 => ("virt", if kvm { "host" } else { "cortex-a57" }, 256),
        GuestArch::Armv7m => unreachable!("bundle_boot refuses an armv7m bundle for qemu-microvm"),
    };
    let memory = format!("{}m", boot.memory_mb.unwrap_or(default_mb));
    let smp = boot.cpus.to_string();

    let mut args: Vec<String> = Vec::new();
    if !np.pinned_cpus.is_empty() {
        // taskset wrapper keeps the whole QEMU thread group on the cell CPUs.
        args.extend([
            "taskset".into(),
            "-c".into(),
            np.pinned_cpus
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
        ]);
    }
    args.push(arch.qemu_bin().into());
    args.extend(
        [
            "-M",
            machine,
            "-cpu",
            cpu,
            "-m",
            &memory,
            "-smp",
            &smp,
            "-nodefaults",
            "-no-user-config",
            "-nographic",
            "-monitor",
            "none",
            "-serial",
            &serial,
            "-kernel",
            kernel,
            "-append",
            &append,
            "-netdev",
            &netdev,
            "-device",
            &nic,
            "-pidfile",
            &pidfile.display().to_string(),
            "-daemonize",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    // QEMU's virtio-mmio is the legacy (version 1) transport unless told otherwise. Linux speaks
    // both; Zephyr's driver refuses version 1, and some drivers speak only it, so the bundle
    // chooses ([`BundleBoot::legacy_mmio`]). A PCI NIC has no virtio-mmio transport to set.
    if !pci {
        let at = args
            .iter()
            .position(|a| a == "-nodefaults")
            .map_or(args.len(), |i| i + 1);
        args.splice(at..at, ["-global".to_string(), transport]);
    }
    // An empty `initrd.img` is a bundle whose kernel needs none: its image carries the whole
    // system. QEMU's arm loader refuses an empty file, so it is not passed at all.
    if std::fs::metadata(initrd).is_ok_and(|m| m.len() > 0) {
        args.extend(["-initrd".to_string(), initrd.to_string()]);
    }
    if kvm {
        args.push("-enable-kvm".into());
    }
    args
}

/// What this node's running QEMU runs on: the emulator names the instruction set, and
/// `-enable-kvm` the acceleration. `None` when the node has no running guest.
pub fn guest_platform(np: &NodePlan, runtime_dir: &Path) -> Option<GuestPlatform> {
    if !matches!(np.backend, Backend::QemuMicrovm | Backend::QemuMcu) {
        return None;
    }
    let pid = guest_pid(np, runtime_dir)?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    platform_of(&cmdline)
}

fn platform_of(cmdline: &[u8]) -> Option<GuestPlatform> {
    let cmdline = String::from_utf8_lossy(cmdline);
    let mut args = cmdline.split('\0');
    let bin = args.next()?;
    let arch = if bin.ends_with("qemu-system-aarch64") {
        GuestArch::Aarch64
    } else if bin.ends_with("qemu-system-x86_64") {
        GuestArch::X86_64
    } else if bin.ends_with(super::qemu_mcu::QEMU_MCU_BIN) {
        GuestArch::Armv7m
    } else {
        return None;
    };
    let acceleration = if args.any(|a| a == "-enable-kvm") {
        Acceleration::Kvm
    } else {
        Acceleration::Tcg
    };
    Some(GuestPlatform { arch, acceleration })
}

fn read_pid(np: &NodePlan, runtime_dir: &Path) -> Option<i32> {
    std::fs::read_to_string(runtime_dir.join(format!("{}.pid", np.runtime_name())))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// This node's QEMU. The pidfile's PID counts only if it is that process: a pidfile outlives a
/// host reboot or a crash, and the kernel reuses PIDs, so a live PID alone could be anything, and
/// `down` would kill it.
fn guest_pid(np: &NodePlan, runtime_dir: &Path) -> Option<i32> {
    let from_pidfile = read_pid(np, runtime_dir).filter(|pid| {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .is_ok_and(|cmdline| is_guest_of(&cmdline, &np.node_id))
    });
    // The pidfile belongs to the runtime dir of the daemon that booted the guest. Another daemon
    // has none, and reading that as "down" removed the gateway from under a running guest.
    from_pidfile.or_else(|| {
        super::foreign::guests()
            .find(|(_, node)| node == &np.node_id)
            .map(|(pid, _)| pid)
    })
}

/// Whether a NUL-separated `/proc/<pid>/cmdline` is QEMU booting `node`, which
/// [`kernel_cmdline`] marks with `vx.node=`, and a `qemu-mcu` guest with `arg=vx.node=` in
/// `-semihosting-config`.
fn is_guest_of(cmdline: &[u8], node: &str) -> bool {
    let cmdline = String::from_utf8_lossy(cmdline);
    let mut args = cmdline.split('\0');
    let marker = format!("vx.node={node}");
    args.next().is_some_and(|bin| bin.contains("qemu"))
        && args.any(|arg| super::foreign::guest_words(arg).any(|word| word == marker))
}

fn microvm_down(np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
    if let Some(pid) = guest_pid(np, runtime_dir) {
        // TERM, brief grace, then KILL; both are fine for a stateless guest.
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .output();
        std::thread::sleep(std::time::Duration::from_millis(300));
        if Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
        }
    }
    let _ = std::fs::remove_file(runtime_dir.join(format!("{}.pid", np.runtime_name())));
    Ok(())
}

fn microvm_status(np: &NodePlan, runtime_dir: &Path) -> NodeState {
    match guest_pid(np, runtime_dir) {
        Some(_) => NodeState::Up,
        None => NodeState::Down,
    }
}

// ---------------------------------------------------------------------------
// qemu-mcu (microcontroller firmware, pidfile-managed like qemu-microvm)
// ---------------------------------------------------------------------------

fn mcu_up(sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
    if microvm_status(np, runtime_dir) == NodeState::Up {
        return Ok(()); // idempotent
    }
    let dir = np
        .bundle
        .as_deref()
        .expect("plan invariant: vm backends carry a bundle");
    let firmware = super::qemu_mcu::firmware_path(np, dir)?;
    let board = super::qemu_mcu::bundle_board(np, dir)?;
    if !on_path(super::qemu_mcu::QEMU_MCU_BIN) {
        return Err(ZoneBackendError::Runtime {
            node: np.node_id.clone(),
            detail: format!(
                "a qemu-mcu node needs `{}` on PATH",
                super::qemu_mcu::QEMU_MCU_BIN
            ),
        });
    }
    let args = super::qemu_mcu::argv(np, runtime_dir, &firmware, board);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    // As `microvm_up`: QEMU needs no privilege beyond its own tap, so it is escalated only to enter
    // a confined netns.
    let qemu_sh = Shell {
        netns: sh.netns.clone(),
        escalate: sh.escalate && sh.netns.is_some(),
    };
    qemu_sh.run(argv[0], &argv[1..])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// libvirt (virsh, qemu:///system)
// ---------------------------------------------------------------------------

const VIRSH: [&str; 2] = ["-c", "qemu:///system"];

/// `virsh` as vxedged's own user, never through `sudo` and never inside the gateway's netns
/// (docs repo `ARCHITECTURE.md` §9). `virsh` is only a client: `libvirtd` does
/// the work, in the host namespace, and authorizes the caller by access to its socket, which
/// membership in the `libvirt` group grants. So escalating it changed nothing it could create
/// and only widened what vxedged runs as root; entering a netns changed nothing either. This is
/// the same authorization the `virt` crate would have relied on, without its LGPL-2.1 code in
/// the shipped binaries or a `libvirt-dev` build dependency.
///
/// Run directly, not through [`Shell`]: `Shell` reads any "Permission denied" as a missing
/// `CAP_NET_ADMIN`, which for `virsh` hid libvirt's real error (found live 2026-10-29: qemu
/// unable to read a guest kernel was reported as a netlink privilege problem). libvirt's own
/// message is kept, with a hint for the two causes that have one.
fn virsh(args: &[&str]) -> Result<String, NetOpsError> {
    let mut full: Vec<&str> = VIRSH.to_vec();
    full.extend_from_slice(args);
    let out = std::process::Command::new("virsh")
        .args(&full)
        .output()
        .map_err(|source| NetOpsError::Spawn {
            program: "virsh".to_string(),
            source,
        })?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(NetOpsError::Command {
        cmdline: format!("virsh {}", full.join(" ")),
        stderr: match virsh_hint(&stderr) {
            Some(hint) => format!("{stderr} ({hint})"),
            None => stderr,
        },
    })
}

/// The fix for a `virsh` failure whose cause libvirt's wording identifies.
fn virsh_hint(stderr: &str) -> Option<&'static str> {
    if stderr.contains("libvirt-sock") || stderr.contains("authentication unavailable") {
        Some(
            "vxedged reaches libvirt as its own user, not through sudo: add that user to the \
             `libvirt` group, `sudo usermod -aG libvirt <user>`, and start a new login session",
        )
    } else if stderr.contains("Permission denied") {
        Some(
            "the guest runs as libvirt's qemu user (`libvirt-qemu` on Debian/Ubuntu), which must \
             be able to read the bundle's kernel and initrd and traverse every directory above \
             them, e.g. `setfacl -m u:libvirt-qemu:x <each parent dir>`",
        )
    } else {
        None
    }
}

fn libvirt_domain_xml(np: &NodePlan, kernel: &str, initrd: &str) -> String {
    let name = np.runtime_name();
    let cmdline = kernel_cmdline(np);
    let cputune = if np.pinned_cpus.is_empty() {
        String::new()
    } else {
        // vcpupin alone leaves QEMU's emulator, I/O and vhost threads on every host CPU.
        // emulatorpin keeps them on the cell too, as `taskset` does for qemu-microvm and Cloud
        // Hypervisor.
        let cpus = np
            .pinned_cpus
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "<cputune><vcpupin vcpu='0' cpuset='{cpus}'/><emulatorpin cpuset='{cpus}'/></cputune>"
        )
    };
    format!(
        r#"<domain type='kvm'>
  <name>{name}</name>
  <memory unit='MiB'>128</memory>
  <vcpu>1</vcpu>
  {cputune}
  <cpu mode='host-passthrough'/>
  <features><acpi/><apic/></features>
  <os>
    <type arch='x86_64' machine='q35'>hvm</type>
    <kernel>{kernel}</kernel>
    <initrd>{initrd}</initrd>
    <cmdline>{cmdline}</cmdline>
  </os>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    <interface type='ethernet'>
      <target dev='{endpoint}' managed='no'/>
      <model type='virtio'/>
    </interface>
    <serial type='file'><source path='/tmp/{name}.serial.log'/><target port='0'/></serial>
    <console type='file'><source path='/tmp/{name}.serial.log'/><target type='serial' port='0'/></console>
  </devices>
</domain>"#,
        endpoint = np.endpoint,
    )
}

fn libvirt_up(np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
    if libvirt_status(np) == NodeState::Up {
        return Ok(()); // idempotent
    }
    let (kernel, initrd) = bundle_paths(np)?;
    let xml_path = runtime_dir.join(format!("{}.xml", np.runtime_name()));
    std::fs::write(&xml_path, libvirt_domain_xml(np, &kernel, &initrd)).map_err(|e| {
        ZoneBackendError::Runtime {
            node: np.node_id.clone(),
            detail: format!("cannot write domain xml: {e}"),
        }
    })?;
    // Not running, but possibly still defined: by an `up` whose start failed, or from an older
    // spec. Clear it first, or `define` refuses with "already exists" and `up` never converges.
    libvirt_down(np)?;
    virsh(&["define", &xml_path.display().to_string()])?;
    virsh(&["start", &np.runtime_name()])?;
    Ok(())
}

fn libvirt_down(np: &NodePlan) -> Result<(), ZoneBackendError> {
    let name = np.runtime_name();
    let _ = virsh(&["destroy", &name]); // tolerate not-running
    let _ = virsh(&["undefine", &name]); // tolerate not-defined
    Ok(())
}

fn libvirt_status(np: &NodePlan) -> NodeState {
    match virsh(&["domstate", &np.runtime_name()]) {
        Ok(s) if s.trim() == "running" => NodeState::Up,
        _ => NodeState::Down,
    }
}

// ---------------------------------------------------------------------------
// the seam
// ---------------------------------------------------------------------------

/// One packaging backend: how a node is started, stopped and observed.
///
/// The contract every implementation owes is stated once, in the
/// `zone-backend-contract` skill, and is not restated here — two statements of one contract
/// drift. The parts that bind an implementor:
///
/// - **Attachment is identical across backends.** The gateway has already created this node's
///   port (`NodePlan.endpoint`/`peer`); a backend consumes it and never names or renames an
///   interface itself. Interface names come from the device id (`gateway::ifnames`), so a
///   backend deriving one would be a second implementation of a rule that must have exactly one.
/// - **`up` and `down` are idempotent and converge.** Running `up` on a live node is a no-op,
///   not an error; `down` on an absent one likewise. Every idempotency bug this project has
///   found lived on a re-run path nobody exercised manually, which is why this is a contract
///   rather than a convention.
/// - **`status` never mutates.** It is called by `zone list` and by the reconciler, including
///   against nodes this process did not start.
/// - **Failures are actionable.** Wrap an implementation's own error in [`ZoneBackendError`]
///   with the node named — a caller matching on a libvirt error would be matching on the
///   implementation this seam exists to hide.
///
/// A backend takes a [`NodePlan`]; it does not compute one. Anything a backend would need to
/// *decide* belongs in `zones::plan`, where it is pure, testable and diffable.
pub trait ZoneBackend {
    /// The `backend:` value that selects this implementation. Used in errors and logs, so it
    /// matches the manifest spelling exactly.
    fn name(&self) -> &'static str;

    fn up(&self, sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError>;

    fn down(&self, sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError>;

    fn status(&self, sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> NodeState;

    /// Where this node's CPU and memory are read (`zones::usage`), or `None` when nothing on
    /// this host runs it: a bare namespace, an attached platform, a node that is down.
    fn usage_source(
        &self,
        _sh: &Shell,
        _np: &NodePlan,
        _runtime_dir: &Path,
    ) -> Option<super::usage::Source> {
        None
    }
}

struct ContainerBackend;
struct QemuMicrovmBackend;
struct LibvirtBackend;
struct CloudHypervisorBackend;
struct WasmCapsuleBackend;
struct QemuMcuBackend;

impl ZoneBackend for ContainerBackend {
    fn name(&self) -> &'static str {
        "container"
    }
    fn up(&self, sh: &Shell, np: &NodePlan, _runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        container_up(sh, np)
    }
    fn down(&self, sh: &Shell, np: &NodePlan, _runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        container_down(sh, np)
    }
    fn status(&self, sh: &Shell, np: &NodePlan, _runtime_dir: &Path) -> NodeState {
        container_status(sh, np)
    }
    fn usage_source(
        &self,
        _sh: &Shell,
        np: &NodePlan,
        _runtime_dir: &Path,
    ) -> Option<super::usage::Source> {
        np.image.as_ref()?;
        let pid = block_on(async {
            let client = ContainerClient::connect_user().await.ok()?;
            client.member_pid(&np.runtime_name(), "payload").await
        })?;
        super::usage::cgroup_of(i32::try_from(pid).ok()?).map(super::usage::Source::Cgroup)
    }
}

impl ZoneBackend for QemuMicrovmBackend {
    fn name(&self) -> &'static str {
        "qemu-microvm"
    }
    fn up(&self, sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        microvm_up(sh, np, runtime_dir)
    }
    fn down(&self, _sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        microvm_down(np, runtime_dir)
    }
    fn status(&self, _sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> NodeState {
        microvm_status(np, runtime_dir)
    }
    fn usage_source(
        &self,
        _sh: &Shell,
        np: &NodePlan,
        runtime_dir: &Path,
    ) -> Option<super::usage::Source> {
        guest_pid(np, runtime_dir).map(super::usage::Source::Process)
    }
}

impl ZoneBackend for QemuMcuBackend {
    fn name(&self) -> &'static str {
        "qemu-mcu"
    }
    fn up(&self, sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        mcu_up(sh, np, runtime_dir)
    }
    // The same pidfile and `vx.node=` marker as a microVM guest.
    fn down(&self, _sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        microvm_down(np, runtime_dir)
    }
    fn status(&self, _sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> NodeState {
        microvm_status(np, runtime_dir)
    }
    fn usage_source(
        &self,
        _sh: &Shell,
        np: &NodePlan,
        runtime_dir: &Path,
    ) -> Option<super::usage::Source> {
        guest_pid(np, runtime_dir).map(super::usage::Source::Process)
    }
}

impl ZoneBackend for LibvirtBackend {
    fn name(&self) -> &'static str {
        "libvirt"
    }
    // The gateway's `Shell` is not used: see [`virsh`].
    fn up(&self, _sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        libvirt_up(np, runtime_dir)
    }
    fn down(
        &self,
        _sh: &Shell,
        np: &NodePlan,
        _runtime_dir: &Path,
    ) -> Result<(), ZoneBackendError> {
        libvirt_down(np)
    }
    fn status(&self, _sh: &Shell, np: &NodePlan, _runtime_dir: &Path) -> NodeState {
        libvirt_status(np)
    }
    // libvirt's QEMU carries the same `vx.node=` marker on its kernel command line.
    fn usage_source(
        &self,
        _sh: &Shell,
        np: &NodePlan,
        runtime_dir: &Path,
    ) -> Option<super::usage::Source> {
        guest_pid(np, runtime_dir).map(super::usage::Source::Process)
    }
}

impl ZoneBackend for CloudHypervisorBackend {
    fn name(&self) -> &'static str {
        "cloud-hypervisor"
    }
    fn up(&self, sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        cloud_hypervisor::up(sh, np, runtime_dir)
    }
    fn down(&self, _sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        cloud_hypervisor::down(np, runtime_dir)
    }
    fn status(&self, _sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> NodeState {
        cloud_hypervisor::status(np, runtime_dir)
    }
    fn usage_source(
        &self,
        _sh: &Shell,
        np: &NodePlan,
        runtime_dir: &Path,
    ) -> Option<super::usage::Source> {
        cloud_hypervisor::vmm_pid(np, runtime_dir).map(super::usage::Source::Process)
    }
}

impl ZoneBackend for WasmCapsuleBackend {
    fn name(&self) -> &'static str {
        "wasm-capsule"
    }
    fn up(&self, sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        wasm_capsule::up(sh, np, runtime_dir)
    }
    fn down(&self, sh: &Shell, np: &NodePlan, _runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        wasm_capsule::down(sh, np)
    }
    fn status(&self, _sh: &Shell, np: &NodePlan, _runtime_dir: &Path) -> NodeState {
        wasm_capsule::status(np)
    }
    fn usage_source(
        &self,
        _sh: &Shell,
        np: &NodePlan,
        _runtime_dir: &Path,
    ) -> Option<super::usage::Source> {
        wasm_capsule::runner_pid(np).map(super::usage::Source::Process)
    }
}

/// **The only place a backend kind is matched.** Everything downstream holds a
/// `&dyn ZoneBackend`, so adding a fourth backend is one impl and one arm here — it used to be
/// three matches in three functions with nothing saying they had to agree.
pub fn backend_for(kind: Backend) -> &'static dyn ZoneBackend {
    match kind {
        Backend::Container => &ContainerBackend,
        Backend::QemuMicrovm => &QemuMicrovmBackend,
        Backend::Libvirt => &LibvirtBackend,
        Backend::CloudHypervisor => &CloudHypervisorBackend,
        Backend::WasmCapsule => &WasmCapsuleBackend,
        Backend::Attach => &super::attach::AttachBackend,
        Backend::QemuMcu => &QemuMcuBackend,
    }
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

/// Resolves `np.remote_bundle` (a published, not-yet-fetched part — see [`NodePlan`]'s own doc)
/// into a real `bundle` path via [`crate::registry::fetch`] before dispatching to a backend, so
/// no [`ZoneBackend`] implementation ever needs to know a bundle can come from a registry.
/// Blocking (shells out to `oras pull`): every caller already runs this off the async runtime
/// (`ZoneSvc`/`ComputeSvc::lifecycle`'s own `blocking(...)` wrapper), matching
/// `NosCheckSvc::resolve`'s `tokio::task::spawn_blocking` precedent for the same fetch.
pub fn node_up(
    sh: &Shell,
    np: &NodePlan,
    runtime_dir: &Path,
    cache_dir: &Path,
) -> Result<(), ZoneBackendError> {
    let resolved;
    let np = if let Some((reference, digest)) = &np.remote_bundle {
        let pulled_dir =
            crate::registry::fetch(reference, digest, cache_dir).map_err(|source| {
                ZoneBackendError::Fetch {
                    node: np.node_id.clone(),
                    reference: reference.clone(),
                    source,
                }
            })?;
        resolved = match np.backend {
            Backend::WasmCapsule => NodePlan {
                module: Some(wasm_capsule::module_in(np, &pulled_dir)?),
                remote_bundle: None,
                ..np.clone()
            },
            _ => NodePlan {
                bundle: Some(pulled_dir),
                remote_bundle: None,
                ..np.clone()
            },
        };
        &resolved
    } else {
        np
    };
    backend_for(np.backend).up(sh, np, runtime_dir)
}

pub fn node_down(sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
    backend_for(np.backend).down(sh, np, runtime_dir)
}

pub fn node_status(sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> NodeState {
    backend_for(np.backend).status(sh, np, runtime_dir)
}

/// Where `np`'s CPU and memory are read, if anything on this host runs it.
pub fn node_usage_source(
    sh: &Shell,
    np: &NodePlan,
    runtime_dir: &Path,
) -> Option<super::usage::Source> {
    backend_for(np.backend).usage_source(sh, np, runtime_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_backend_kind_selects_the_implementation_it_names() {
        assert_eq!(backend_for(Backend::Container).name(), "container");
        assert_eq!(backend_for(Backend::QemuMicrovm).name(), "qemu-microvm");
        assert_eq!(backend_for(Backend::Libvirt).name(), "libvirt");
        assert_eq!(
            backend_for(Backend::CloudHypervisor).name(),
            "cloud-hypervisor"
        );
        assert_eq!(backend_for(Backend::WasmCapsule).name(), "wasm-capsule");
        assert_eq!(backend_for(Backend::QemuMcu).name(), "qemu-mcu");
    }

    /// libvirt's wording decides the hint: socket refusal (not in the `libvirt` group), a guest
    /// file qemu cannot read, or neither. The first two forms are libvirt's own messages.
    #[test]
    fn virsh_failures_get_the_hint_their_cause_needs() {
        let socket = virsh_hint(
            "error: failed to connect to the hypervisor\nerror: Failed to connect socket to \
             '/var/run/libvirt/libvirt-sock': Permission denied",
        );
        assert!(socket.is_some_and(|h| h.contains("usermod -aG libvirt")));
        let polkit = virsh_hint("error: authentication unavailable: no polkit agent available");
        assert!(polkit.is_some_and(|h| h.contains("usermod -aG libvirt")));
        let file = virsh_hint(
            "error: Failed to start domain 'vx-node-libvirt-01'\nerror: internal error: qemu \
             unexpectedly closed the monitor: Could not open '/srv/ws/vmlinuz': Permission denied",
        );
        assert!(file.is_some_and(|h| h.contains("libvirt-qemu")));
        assert_eq!(virsh_hint("error: Domain not found"), None);
    }

    fn image_node() -> NodePlan {
        NodePlan {
            zone_id: "front-left".into(),
            node_id: format!("node-never-{}", std::process::id()),
            role: crate::model::DeviceRole::Ecu,
            backend: Backend::Container,
            endpoint: "vz-never".into(),
            peer: "vzp-never".into(),
            kind: crate::gateway::plan::EndpointKind::Veth,
            vlan: 101,
            slice: Some("adas".into()),
            ip_cidr: Some("10.101.0.9/24".into()),
            extra_cidrs: Vec::new(),
            bundle: None,
            remote_bundle: None,
            image: Some(PAUSE_IMAGE.into()),
            module: None,
            pinned_cpus: Vec::new(),
            qemu: Default::default(),
        }
    }

    fn microvm_node(bundle: Option<std::path::PathBuf>) -> NodePlan {
        NodePlan {
            backend: Backend::QemuMicrovm,
            endpoint: "vz-arm".into(),
            peer: "vz-arm".into(),
            kind: crate::gateway::plan::EndpointKind::Tap,
            bundle,
            image: None,
            ..image_node()
        }
    }

    fn boot(arch: GuestArch) -> BundleBoot {
        BundleBoot {
            arch,
            legacy_mmio: false,
            cpus: 1,
            memory_mb: None,
            virtualization: false,
            virtio_pci: false,
        }
    }

    fn arg_after<'a>(argv: &'a [String], flag: &str) -> &'a str {
        let i = argv.iter().position(|a| a == flag).expect(flag);
        &argv[i + 1]
    }

    fn qemu(machine: QemuMachine, transport: VirtioTransport, vhost: bool) -> QemuOptions {
        QemuOptions {
            machine,
            transport,
            vhost,
        }
    }

    /// With no `qemu:` block an x86 guest gets exactly the microVM command line it had before the
    /// block existed, so no existing experiment changes.
    #[test]
    fn no_qemu_block_keeps_the_microvm_command_line() {
        let np = microvm_node(None);
        let rt = Path::new("/run/vx");
        let name = np.runtime_name();
        let argv = microvm_argv(
            &np,
            rt,
            "k",
            "i",
            boot(GuestArch::X86_64),
            Acceleration::Kvm,
        );
        let expected: Vec<String> = [
            "qemu-system-x86_64",
            "-M",
            "microvm,acpi=off",
            "-cpu",
            "host",
            "-m",
            "128m",
            "-smp",
            "1",
            "-nodefaults",
            "-global",
            "virtio-mmio.force-legacy=false",
            "-no-user-config",
            "-nographic",
            "-monitor",
            "none",
            "-serial",
            &format!("file:/run/vx/{name}.serial.log"),
            "-kernel",
            "k",
            "-append",
            &kernel_cmdline_for(&np, GuestArch::X86_64),
            "-netdev",
            "tap,id=n0,ifname=vz-arm,script=no,downscript=no",
            "-device",
            &format!("virtio-net-device,netdev=n0,mac={}", np.guest_mac()),
            "-pidfile",
            &format!("/run/vx/{name}.pid"),
            "-daemonize",
            "-enable-kvm",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(argv, expected);
    }

    /// Each `qemu:` arm builds its machine, NIC and data path, and only an MMIO NIC sets the
    /// virtio-mmio transport.
    #[test]
    fn each_qemu_arm_builds_its_machine_nic_and_data_path() {
        use QemuMachine::{Microvm, Q35};
        use VirtioTransport::{Mmio, Pci};
        let rt = Path::new("/run/vx");
        for (options, machine, nic, vhost) in [
            (
                qemu(Microvm, Mmio, false),
                "microvm,acpi=off",
                "virtio-net-device",
                false,
            ),
            (
                qemu(Microvm, Mmio, true),
                "microvm,acpi=off",
                "virtio-net-device",
                true,
            ),
            (
                qemu(Microvm, Pci, false),
                "microvm,pcie=on,acpi=on",
                "virtio-net-pci",
                false,
            ),
            (qemu(Q35, Pci, false), "q35", "virtio-net-pci", false),
            (qemu(Q35, Pci, true), "q35", "virtio-net-pci", true),
        ] {
            let mut np = microvm_node(None);
            np.qemu = options;
            let argv = microvm_argv(
                &np,
                rt,
                "k",
                "i",
                boot(GuestArch::X86_64),
                Acceleration::Kvm,
            );
            assert_eq!(arg_after(&argv, "-M"), machine, "{options:?}");
            assert!(arg_after(&argv, "-device").starts_with(&format!("{nic},netdev=n0,mac=")));
            assert_eq!(
                arg_after(&argv, "-netdev").ends_with(",vhost=on"),
                vhost,
                "{options:?}"
            );
            assert_eq!(
                argv.iter().any(|a| a == "-global"),
                options.transport == Mmio,
                "{options:?}"
            );
            assert_eq!(arg_after(&argv, "-cpu"), "host");
            assert_eq!(arg_after(&argv, "-m"), "128m");
        }
    }

    /// What the manifest cannot settle is refused at boot: options on a non-x86 guest, a PCI NIC
    /// for a guest that drives none, vhost without `/dev/vhost-net`.
    #[test]
    fn qemu_options_the_bundle_or_host_cannot_honour_are_refused_at_boot() {
        use QemuMachine::{Microvm, Q35};
        use VirtioTransport::{Mmio, Pci};
        let yes: fn() -> bool = || true;
        let no: fn() -> bool = || false;
        let pci_bundle = BundleBoot {
            virtio_pci: true,
            ..boot(GuestArch::X86_64)
        };
        let mut np = microvm_node(None);

        // The default needs nothing, on any guest and any host.
        for arch in [GuestArch::X86_64, GuestArch::Aarch64] {
            check_qemu_options(&np, boot(arch), no).unwrap();
        }

        np.qemu = qemu(Q35, Pci, false);
        check_qemu_options(&np, pci_bundle, no).unwrap();
        let e = check_qemu_options(&np, boot(GuestArch::X86_64), yes).unwrap_err();
        assert!(e.to_string().contains("org.vxedge.virtio-pci"), "{e}");
        let arm = BundleBoot {
            arch: GuestArch::Aarch64,
            ..pci_bundle
        };
        let e = check_qemu_options(&np, arm, yes).unwrap_err();
        assert!(e.to_string().contains("aarch64"), "{e}");

        np.qemu = qemu(Microvm, Mmio, true);
        check_qemu_options(&np, boot(GuestArch::X86_64), yes).unwrap();
        let e = check_qemu_options(&np, boot(GuestArch::X86_64), no).unwrap_err();
        assert!(e.to_string().contains("/dev/vhost-net"), "{e}");
    }

    /// A bundle declares a PCI-capable guest with the published annotation.
    #[test]
    fn a_bundle_declares_a_pci_capable_guest() {
        let dir = tempfile::tempdir().unwrap();
        let np = microvm_node(Some(dir.path().to_path_buf()));
        for (meta, pci) in [
            (r#"{}"#, false),
            (r#"{"org.vxedge.virtio-pci":"true"}"#, true),
            (r#"{"org.vxedge.virtio-pci":true}"#, true),
            (r#"{"org.vxedge.virtio-pci":"false"}"#, false),
        ] {
            std::fs::write(dir.path().join("meta.json"), meta).unwrap();
            assert_eq!(bundle_boot(&np).unwrap().virtio_pci, pci, "{meta}");
        }
        std::fs::write(
            dir.path().join("meta.json"),
            r#"{"org.vxedge.virtio-pci":"yes"}"#,
        )
        .unwrap();
        assert!(
            bundle_boot(&np)
                .unwrap_err()
                .to_string()
                .contains("not true or false")
        );
    }

    /// An aarch64 bundle boots QEMU's `virt` machine with the PL011 console. Without KVM for its
    /// instruction set it runs under the emulator, so `-enable-kvm` is absent.
    #[test]
    fn an_aarch64_guest_boots_virt_with_its_console_under_tcg() {
        let np = microvm_node(None);
        let rt = Path::new("/run/vx");
        let argv = microvm_argv(
            &np,
            rt,
            "k",
            "i",
            boot(GuestArch::Aarch64),
            Acceleration::Tcg,
        );
        assert_eq!(argv[0], "qemu-system-aarch64");
        assert_eq!(arg_after(&argv, "-M"), "virt");
        assert_eq!(arg_after(&argv, "-cpu"), "cortex-a57");
        assert_eq!(arg_after(&argv, "-m"), "256m");
        assert!(arg_after(&argv, "-append").starts_with("console=ttyAMA0 "));
        assert!(arg_after(&argv, "-append").contains(&format!("vx.node={}", np.node_id)));
        assert_eq!(
            arg_after(&argv, "-device"),
            format!("virtio-net-device,netdev=n0,mac={}", np.guest_mac())
        );
        assert!(!argv.iter().any(|a| a == "-enable-kvm"));

        let x86 = microvm_argv(
            &np,
            rt,
            "k",
            "i",
            boot(GuestArch::X86_64),
            Acceleration::Kvm,
        );
        assert_eq!(x86[0], "qemu-system-x86_64");
        assert_eq!(arg_after(&x86, "-M"), "microvm,acpi=off");
        assert_eq!(arg_after(&x86, "-cpu"), "host");
        assert!(arg_after(&x86, "-append").starts_with("console=ttyS0 "));
        assert_eq!(x86.last().map(String::as_str), Some("-enable-kvm"));
        assert!(x86.iter().any(|a| a == "-daemonize"));
    }

    /// A bundle's initrd is passed when it has content, and left out when it is the empty stub
    /// of a kernel that needs none.
    #[test]
    fn an_empty_initrd_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let (full, empty) = (dir.path().join("full.img"), dir.path().join("empty.img"));
        std::fs::write(&full, b"initramfs").unwrap();
        std::fs::write(&empty, b"").unwrap();
        let np = microvm_node(None);
        let rt = Path::new("/run/vx");
        let with = microvm_argv(
            &np,
            rt,
            "k",
            full.to_str().unwrap(),
            boot(GuestArch::Aarch64),
            Acceleration::Tcg,
        );
        assert_eq!(arg_after(&with, "-initrd"), full.to_str().unwrap());
        let without = microvm_argv(
            &np,
            rt,
            "k",
            empty.to_str().unwrap(),
            boot(GuestArch::Aarch64),
            Acceleration::Tcg,
        );
        assert!(!without.iter().any(|a| a == "-initrd"));
    }

    /// The bundle picks the virtio-mmio transport: modern by default, legacy when it says so.
    #[test]
    fn a_bundle_picks_its_virtio_mmio_transport() {
        let dir = tempfile::tempdir().unwrap();
        let np = microvm_node(Some(dir.path().to_path_buf()));
        let rt = Path::new("/run/vx");
        for (meta, legacy) in [
            (r#"{"arch":"aarch64"}"#, false),
            (r#"{"arch":"aarch64","virtio_mmio":"modern"}"#, false),
            (r#"{"arch":"aarch64","virtio_mmio":"legacy"}"#, true),
        ] {
            std::fs::write(dir.path().join("meta.json"), meta).unwrap();
            let b = bundle_boot(&np).unwrap();
            assert_eq!(b.legacy_mmio, legacy, "{meta}");
            let argv = microvm_argv(&np, rt, "k", "i", b, Acceleration::Tcg);
            assert_eq!(
                arg_after(&argv, "-global"),
                format!("virtio-mmio.force-legacy={legacy}")
            );
        }
        std::fs::write(dir.path().join("meta.json"), r#"{"virtio_mmio":"v3"}"#).unwrap();
        assert!(bundle_boot(&np).unwrap_err().to_string().contains("v3"));
    }

    /// A bundle sizes its guest and, on aarch64, asks for the virtualization extensions.
    #[test]
    fn a_bundle_sizes_its_guest_and_may_ask_for_el2() {
        let dir = tempfile::tempdir().unwrap();
        let mut np = microvm_node(Some(dir.path().to_path_buf()));
        let rt = Path::new("/run/vx");
        std::fs::write(
            dir.path().join("meta.json"),
            r#"{"arch":"aarch64","cpus":2,"memory_mb":2048,"virtualization":true}"#,
        )
        .unwrap();
        let b = bundle_boot(&np).unwrap();
        let argv = microvm_argv(&np, rt, "k", "i", b, Acceleration::Tcg);
        assert_eq!(
            arg_after(&argv, "-M"),
            "virt,virtualization=on,gic-version=3"
        );
        assert_eq!(arg_after(&argv, "-smp"), "2");
        assert_eq!(arg_after(&argv, "-m"), "2048m");

        // Defaults: one vCPU, the architecture's memory, no EL2.
        std::fs::write(dir.path().join("meta.json"), r#"{"arch":"aarch64"}"#).unwrap();
        let argv = microvm_argv(
            &np,
            rt,
            "k",
            "i",
            bundle_boot(&np).unwrap(),
            Acceleration::Tcg,
        );
        assert_eq!(arg_after(&argv, "-M"), "virt");
        assert_eq!(arg_after(&argv, "-smp"), "1");
        assert_eq!(arg_after(&argv, "-m"), "256m");

        for (meta, why) in [
            (r#"{"virtualization":true}"#, "aarch64 only"),
            (r#"{"arch":"aarch64","cpus":0}"#, "0 cpus"),
            (r#"{"arch":"aarch64","cpus":"two"}"#, "not a whole number"),
            (r#"{"arch":"aarch64","memory_mb":16}"#, "16 MiB"),
            (
                r#"{"arch":"aarch64","virtualization":"yes"}"#,
                "not true or false",
            ),
        ] {
            std::fs::write(dir.path().join("meta.json"), meta).unwrap();
            let e = bundle_boot(&np).unwrap_err().to_string();
            assert!(e.contains(why), "{meta}: {e}");
        }

        // A pinned node gets no more vCPUs than it has pinned CPUs.
        np.pinned_cpus = vec![3];
        std::fs::write(
            dir.path().join("meta.json"),
            r#"{"arch":"aarch64","cpus":2}"#,
        )
        .unwrap();
        assert!(
            bundle_boot(&np)
                .unwrap_err()
                .to_string()
                .contains("pinned to 1")
        );
    }

    /// KVM runs a guest only on a host of the same instruction set.
    #[test]
    fn kvm_only_for_the_hosts_own_instruction_set() {
        let host = GuestArch::host().expect("a supported test host");
        let other = match host {
            GuestArch::X86_64 => GuestArch::Aarch64,
            GuestArch::Aarch64 | GuestArch::Armv7m => GuestArch::X86_64,
        };
        assert_eq!(Acceleration::for_guest(host, true), Acceleration::Kvm);
        assert_eq!(Acceleration::for_guest(host, false), Acceleration::Tcg);
        assert_eq!(Acceleration::for_guest(other, true), Acceleration::Tcg);
        assert_eq!(
            Acceleration::for_guest(GuestArch::Armv7m, true),
            Acceleration::Tcg,
            "no host runs a microcontroller under KVM"
        );
    }

    /// Status reads the running emulator, not the bundle: what started is what is reported.
    #[test]
    fn a_running_guests_platform_comes_from_its_command_line() {
        let tcg = b"/usr/bin/qemu-system-aarch64\0-M\0virt\0-append\0console=ttyAMA0 vx.node=a\0";
        assert_eq!(
            platform_of(tcg),
            Some(GuestPlatform {
                arch: GuestArch::Aarch64,
                acceleration: Acceleration::Tcg
            })
        );
        let kvm = b"qemu-system-x86_64\0-M\0microvm,acpi=off\0-daemonize\0-enable-kvm\0";
        assert_eq!(
            platform_of(kvm),
            Some(GuestPlatform {
                arch: GuestArch::X86_64,
                acceleration: Acceleration::Kvm
            })
        );
        let mcu = b"qemu-system-arm\0-M\0mps2-an385\0-semihosting-config\0enable=on,target=native,arg=vx.ip=10.0.0.5/24,arg=vx.node=m\0";
        assert_eq!(
            platform_of(mcu),
            Some(GuestPlatform {
                arch: GuestArch::Armv7m,
                acceleration: Acceleration::Tcg
            })
        );
        assert_eq!(platform_of(b"taskset\0-c\0"), None);
    }

    /// A microcontroller bundle on `qemu-microvm` is refused with where it does run.
    #[test]
    fn qemu_microvm_points_an_mcu_bundle_at_qemu_mcu() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("meta.json"), r#"{"arch":"armv7m"}"#).unwrap();
        let np = microvm_node(Some(dir.path().to_path_buf()));
        let e = bundle_boot(&np).unwrap_err().to_string();
        assert!(e.contains("backend: qemu-mcu"), "{e}");
    }

    /// A bundle names its instruction set in `meta.json`; one that names none is x86_64.
    #[test]
    fn a_bundles_arch_comes_from_its_meta_json() {
        let dir = tempfile::tempdir().unwrap();
        let np = microvm_node(Some(dir.path().to_path_buf()));
        assert_eq!(
            bundle_boot(&np).unwrap().arch,
            GuestArch::X86_64,
            "no meta.json"
        );
        for (meta, want) in [
            (r#"{"bundle":"linux-probe"}"#, GuestArch::X86_64),
            (r#"{"arch":"x86_64"}"#, GuestArch::X86_64),
            (r#"{"arch":"aarch64"}"#, GuestArch::Aarch64),
            (r#"{"arch":"arm64"}"#, GuestArch::Aarch64),
            (r#"{"org.vxedge.arch":"aarch64"}"#, GuestArch::Aarch64),
            (r#"{"org.vxedge.arch":"x86_64"}"#, GuestArch::X86_64),
            (r#"{"org.vxedge.arch":"i386"}"#, GuestArch::X86_64),
        ] {
            std::fs::write(dir.path().join("meta.json"), meta).unwrap();
            assert_eq!(bundle_boot(&np).unwrap().arch, want, "{meta}");
        }
        std::fs::write(dir.path().join("meta.json"), r#"{"arch":"riscv64"}"#).unwrap();
        let e = bundle_boot(&np).unwrap_err().to_string();
        assert!(e.contains("riscv64"), "{e}");
    }

    /// The guest sees the host CPU, as on qemu-microvm (`-cpu host` under KVM). Without a `<cpu>`
    /// element libvirt picks `qemu64`, and a guest that needs the host's CPU features never boots.
    #[test]
    fn a_libvirt_guest_gets_the_host_cpu() {
        let np = NodePlan {
            backend: Backend::Libvirt,
            image: None,
            ..image_node()
        };
        let xml = libvirt_domain_xml(&np, "/b/vmlinuz", "/b/initrd.img");
        assert!(xml.contains("<cpu mode='host-passthrough'/>"), "{xml}");
    }

    /// A q35 guest needs ACPI: without `<features><acpi/>` QEMU runs it with `acpi=off`, and the
    /// virtio-net behind its PCIe root port stays in D3cold, so the guest boots with no `eth0`
    /// (found live on vx-vehicle-04, 2026-11-10).
    #[test]
    fn a_libvirt_guest_gets_acpi_so_its_pcie_nic_powers_up() {
        let np = NodePlan {
            backend: Backend::Libvirt,
            image: None,
            ..image_node()
        };
        let xml = libvirt_domain_xml(&np, "/b/vmlinuz", "/b/initrd.img");
        assert!(xml.contains("<features><acpi/><apic/></features>"), "{xml}");
    }

    /// A pinned libvirt node keeps every QEMU thread on its cell: the vCPU (`vcpupin`) and the
    /// emulator, I/O and vhost threads (`emulatorpin`). With `vcpupin` alone those ran on every
    /// host CPU (measured on vx-vehicle-04). An unpinned node gets no `<cputune>`.
    #[test]
    fn a_pinned_libvirt_node_pins_its_emulator_threads_too() {
        let mut np = NodePlan {
            backend: Backend::Libvirt,
            image: None,
            ..image_node()
        };
        assert!(!libvirt_domain_xml(&np, "/b/vmlinuz", "/b/initrd.img").contains("<cputune>"));
        np.pinned_cpus = vec![6, 7];
        let xml = libvirt_domain_xml(&np, "/b/vmlinuz", "/b/initrd.img");
        assert!(
            xml.contains(
                "<cputune><vcpupin vcpu='0' cpuset='6,7'/><emulatorpin cpuset='6,7'/></cputune>"
            ),
            "{xml}"
        );
    }

    /// A pinned container node runs its payload and its infra container on the pinned CPUs; an
    /// unpinned one sets no cpuset (measured: the container backend used to drop the pins).
    #[test]
    fn a_pinned_container_node_carries_its_pins_into_both_containers() {
        let mut np = image_node();
        assert!(payload_spec("img", &np.pinned_cpus).cpuset_cpus.is_empty());
        np.pinned_cpus = vec![8];
        assert_eq!(payload_spec("img", &np.pinned_cpus).cpuset_cpus, vec![8]);
        assert_eq!(payload_group(&np).unwrap().infra().cpuset_cpus, vec![8]);
        assert_eq!(
            payload_spec("img", &[]).stop_timeout_s,
            Some(PAYLOAD_STOP_TIMEOUT_S)
        );
    }

    /// `status` reports and never fails: with no socket (CI) or no such group (a dev host), an
    /// image node that was never started reads down, the same either way.
    #[test]
    fn an_image_node_never_started_reads_down_with_or_without_a_daemon() {
        let sh = Shell {
            netns: None,
            escalate: false,
        };
        assert_eq!(
            container_status(&sh, &image_node()),
            NodeState::Down,
            "no group, no namespace"
        );
    }

    #[test]
    fn wait_until_stops_as_soon_as_the_condition_holds() {
        let mut calls = 0;
        assert!(wait_until(std::time::Duration::from_secs(5), || {
            calls += 1;
            calls == 3
        }));
        assert_eq!(calls, 3);
    }

    #[test]
    fn wait_until_gives_up_at_the_timeout() {
        assert!(!wait_until(std::time::Duration::from_millis(30), || false));
    }

    /// `down` on a node never brought up has no namespace, so it must not wait for a port that
    /// is staying on the gateway: that wait would time out and fail an idempotent call.
    #[test]
    fn down_on_a_node_never_started_does_not_wait_for_its_port() {
        let sh = Shell {
            netns: None,
            escalate: false,
        };
        let np = NodePlan {
            image: None,
            ..image_node()
        };
        let started = std::time::Instant::now();
        container_down(&sh, &np).unwrap();
        assert!(started.elapsed() < PORT_GONE_TIMEOUT);
    }

    #[test]
    fn a_reused_pid_is_not_mistaken_for_the_guest() {
        let qemu = b"qemu-system-x86_64\0-append\0console=ttyS0 vx.node=zone-fl\0";
        assert!(is_guest_of(qemu, "zone-fl"));
        assert!(!is_guest_of(qemu, "zone-f"), "another node's id");
        let mcu = b"qemu-system-arm\0-semihosting-config\0enable=on,target=native,arg=vx.ip=10.0.0.5/24,arg=vx.node=ecu-m3\0";
        assert!(
            is_guest_of(mcu, "ecu-m3"),
            "a qemu-mcu guest's marker is a semihosting arg"
        );
        assert!(!is_guest_of(mcu, "ecu-m"));
        assert!(!is_guest_of(
            b"/usr/bin/bash\0-c\0vx.node=zone-fl\0",
            "zone-fl"
        ));
    }

    /// The seam's two calling contexts: no runtime at all (a plain test, a CLI), and a
    /// `spawn_blocking` thread of the daemon's runtime, which is where `vxedged` calls it.
    #[test]
    fn block_on_drives_a_future_outside_and_off_a_runtime() {
        assert_eq!(block_on(async { 7 }), 7);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let got = rt.block_on(async {
            tokio::task::spawn_blocking(|| {
                block_on(async {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    8
                })
            })
            .await
            .unwrap()
        });
        assert_eq!(got, 8, "timers are driven by the caller's runtime");
    }
}
