//! Node lifecycle per packaging backend. All three attach the node to the
//! gateway port the same way; only the isolation mechanism differs — that is
//! exactly the H3 variable.
//!
//! - `container`: named network namespace; the veth peer moves in, renamed
//!   eth0, addressed from the manifest. The payload is exec'd into the netns
//!   by the experiment runner (or a future OCI integration).
//! - `qemu-microvm`: raw QEMU `microvm` machine, direct kernel boot from the
//!   node's image bundle (vmlinuz + initrd.img), virtio-net on the tap.
//!   Guest init reads `vx.ip=` from the kernel cmdline; payload autostarts.
//! - `libvirt`: full QEMU machine via virsh (qemu:///system), same bundle and
//!   cmdline, precreated tap (`managed='no'`).
//!
//! Ordering contract: `gateway up` before `zone up`; `zone down` before
//! `gateway down`. Deleting a container netns destroys its veth pair, so
//! re-running `vxedge up` (idempotent) restores endpoints after `zone down`.

use std::path::Path;

use super::plan::NodePlan;
use crate::gateway::netops::{NetOpsError, Shell};
use crate::manifest::Backend;

#[derive(Debug, thiserror::Error)]
pub enum ZoneBackendError {
    #[error(transparent)]
    NetOps(#[from] NetOpsError),
    #[error("node `{node}`: bundle file missing: {path}")]
    BundleFile { node: String, path: String },
    #[error("node `{node}`: {detail}")]
    Runtime { node: String, detail: String },
}

/// Observed state of one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Up,
    Down,
}

fn kernel_cmdline(np: &NodePlan) -> String {
    let ip = np.ip_cidr.as_deref().unwrap_or("");
    format!(
        "console=ttyS0 reboot=t panic=-1 quiet vx.ip={ip} vx.node={}",
        np.node_id
    )
}

fn bundle_paths(np: &NodePlan) -> Result<(String, String), ZoneBackendError> {
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

fn container_up(sh: &Shell, np: &NodePlan) -> Result<(), ZoneBackendError> {
    let ns = np.runtime_name();
    // Netns registry ops run in the HOST context (see Shell::run_host);
    // only the veth move runs where the interface currently lives.
    sh.run_host_tolerate_exists("ip", &["netns", "add", &ns])?;
    if sh.link_exists(&np.peer) {
        sh.run("ip", &["link", "set", &np.peer, "netns", &ns])?;
        sh.run_host(
            "ip",
            &[
                "netns", "exec", &ns, "ip", "link", "set", &np.peer, "name", "eth0",
            ],
        )?;
    }
    sh.run_host(
        "ip",
        &["netns", "exec", &ns, "ip", "link", "set", "lo", "up"],
    )?;
    if let Some(cidr) = &np.ip_cidr {
        sh.run_host_tolerate_exists(
            "ip",
            &[
                "netns", "exec", &ns, "ip", "addr", "add", cidr, "dev", "eth0",
            ],
        )?;
    }
    sh.run_host(
        "ip",
        &["netns", "exec", &ns, "ip", "link", "set", "eth0", "up"],
    )?;
    Ok(())
}

fn container_down(sh: &Shell, np: &NodePlan) -> Result<(), ZoneBackendError> {
    // Destroys the moved veth peer (and thus the bridge-side pair);
    // `vxedge up` re-creates endpoints idempotently.
    sh.run_host_tolerate_missing("ip", &["netns", "del", &np.runtime_name()])?;
    Ok(())
}

fn container_status(sh: &Shell, np: &NodePlan) -> NodeState {
    let ns = np.runtime_name();
    let listed = sh.run_host("ip", &["netns", "list"]).unwrap_or_default();
    if listed
        .lines()
        .any(|l| l.split_whitespace().next() == Some(ns.as_str()))
        && sh
            .run_host(
                "ip",
                &[
                    "netns", "exec", &ns, "ip", "-j", "link", "show", "dev", "eth0",
                ],
            )
            .is_ok()
    {
        NodeState::Up
    } else {
        NodeState::Down
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
    let name = np.runtime_name();
    let pidfile = runtime_dir.join(format!("{name}.pid"));
    let serial = format!(
        "file:{}",
        runtime_dir.join(format!("{name}.serial.log")).display()
    );
    let netdev = format!("tap,id=n0,ifname={},script=no,downscript=no", np.endpoint);
    let append = kernel_cmdline(np);
    let kvm = Path::new("/dev/kvm").exists();

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
    args.push("qemu-system-x86_64".into());
    args.extend(
        [
            "-M",
            "microvm,acpi=off",
            "-cpu",
            if kvm { "host" } else { "max" },
            "-m",
            "128m",
            "-smp",
            "1",
            "-nodefaults",
            "-no-user-config",
            "-nographic",
            "-monitor",
            "none",
            "-serial",
            &serial,
            "-kernel",
            &kernel,
            "-initrd",
            &initrd,
            "-append",
            &append,
            "-netdev",
            &netdev,
            "-device",
            "virtio-net-device,netdev=n0",
            "-pidfile",
            &pidfile.display().to_string(),
            "-daemonize",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    if kvm {
        args.push("-enable-kvm".into());
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    sh.run(argv[0], &argv[1..])?;
    Ok(())
}

fn read_pid(np: &NodePlan, runtime_dir: &Path) -> Option<i32> {
    std::fs::read_to_string(runtime_dir.join(format!("{}.pid", np.runtime_name())))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn microvm_down(np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
    if let Some(pid) = read_pid(np, runtime_dir) {
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
    match read_pid(np, runtime_dir) {
        Some(pid) if Path::new(&format!("/proc/{pid}")).exists() => NodeState::Up,
        _ => NodeState::Down,
    }
}

// ---------------------------------------------------------------------------
// libvirt (virsh, qemu:///system)
// ---------------------------------------------------------------------------

const VIRSH: [&str; 2] = ["-c", "qemu:///system"];

fn virsh(sh: &Shell, args: &[&str]) -> Result<String, NetOpsError> {
    let mut full: Vec<&str> = VIRSH.to_vec();
    full.extend_from_slice(args);
    sh.run("virsh", &full)
}

fn libvirt_domain_xml(np: &NodePlan, kernel: &str, initrd: &str) -> String {
    let name = np.runtime_name();
    let cmdline = kernel_cmdline(np);
    let cputune = if np.pinned_cpus.is_empty() {
        String::new()
    } else {
        format!(
            "<cputune><vcpupin vcpu='0' cpuset='{}'/></cputune>",
            np.pinned_cpus
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    format!(
        r#"<domain type='kvm'>
  <name>{name}</name>
  <memory unit='MiB'>128</memory>
  <vcpu>1</vcpu>
  {cputune}
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

fn libvirt_up(sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
    if libvirt_status(sh, np) == NodeState::Up {
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
    virsh(sh, &["define", &xml_path.display().to_string()])?;
    virsh(sh, &["start", &np.runtime_name()])?;
    Ok(())
}

fn libvirt_down(sh: &Shell, np: &NodePlan) -> Result<(), ZoneBackendError> {
    let name = np.runtime_name();
    let _ = virsh(sh, &["destroy", &name]); // tolerate not-running
    let _ = virsh(sh, &["undefine", &name]); // tolerate not-defined
    Ok(())
}

fn libvirt_status(sh: &Shell, np: &NodePlan) -> NodeState {
    match virsh(sh, &["domstate", &np.runtime_name()]) {
        Ok(s) if s.trim() == "running" => NodeState::Up,
        _ => NodeState::Down,
    }
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

pub fn node_up(sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
    match np.backend {
        Backend::Container => container_up(sh, np),
        Backend::QemuMicrovm => microvm_up(sh, np, runtime_dir),
        Backend::Libvirt => libvirt_up(sh, np, runtime_dir),
    }
}

pub fn node_down(sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
    match np.backend {
        Backend::Container => container_down(sh, np),
        Backend::QemuMicrovm => microvm_down(np, runtime_dir),
        Backend::Libvirt => libvirt_down(sh, np),
    }
}

pub fn node_status(sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> NodeState {
    match np.backend {
        Backend::Container => container_status(sh, np),
        Backend::QemuMicrovm => microvm_status(np, runtime_dir),
        Backend::Libvirt => libvirt_status(sh, np),
    }
}
