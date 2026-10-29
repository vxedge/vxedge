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
//!
//! All three are selected through one [`ZoneBackend`] seam, and [`backend_for`] is the only
//! place a backend kind is matched. Adding a fourth is an impl and one arm — see the trait's
//! doc for what an implementation owes.

use std::path::Path;

use super::plan::NodePlan;
use crate::gateway::netlink;
use crate::gateway::netops::{NetOpsError, Shell};
use crate::model::Backend;

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
    #[error("node `{node}`: fetching published bundle `{reference}` failed: {source}")]
    Fetch {
        node: String,
        reference: String,
        #[source]
        source: crate::registry::RegistryFetchError,
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
    // Netns registry ops run in the HOST context — never nested inside another netns, since a
    // bind mount created from inside one doesn't propagate; only the veth move targets where
    // the interface currently lives. See `gateway::netlink`'s own module doc for why netns
    // create/delete is `rtnetlink::NetworkNamespace`, not a hand-rolled unshare/mount.
    netlink::ensure_netns(&ns).map_err(|e| nl(np, e))?;
    if netlink::link_exists(sh.netns.as_deref(), &np.peer) {
        netlink::move_to_netns(sh.netns.as_deref(), &np.peer, &ns).map_err(|e| nl(np, e))?;
        netlink::rename_link(Some(&ns), &np.peer, "eth0").map_err(|e| nl(np, e))?;
    }
    netlink::link_up(Some(&ns), "lo").map_err(|e| nl(np, e))?;
    if let Some(cidr) = &np.ip_cidr {
        netlink::add_address(Some(&ns), "eth0", cidr).map_err(|e| nl(np, e))?;
    }
    netlink::link_up(Some(&ns), "eth0").map_err(|e| nl(np, e))?;
    Ok(())
}

fn container_down(_sh: &Shell, np: &NodePlan) -> Result<(), ZoneBackendError> {
    // Destroys the moved veth peer (and thus the bridge-side pair);
    // `vxedge up` re-creates endpoints idempotently.
    netlink::delete_netns(&np.runtime_name()).map_err(|e| nl(np, e))
}

fn container_status(_sh: &Shell, np: &NodePlan) -> NodeState {
    let ns = np.runtime_name();
    if netlink::netns_exists(&ns) && netlink::link_exists(Some(&ns), "eth0") {
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

/// `virsh` as vxedged's own user, never through `sudo` and never inside the gateway's netns
/// (Track V0, `docs-internal/PROVISIONING.md` §10). `virsh` is only a client: `libvirtd` does
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
}

struct ContainerBackend;
struct QemuMicrovmBackend;
struct LibvirtBackend;

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
}

/// **The only place a backend kind is matched.** Everything downstream holds a
/// `&dyn ZoneBackend`, so adding a fourth backend is one impl and one arm here — it used to be
/// three matches in three functions with nothing saying they had to agree.
pub fn backend_for(kind: Backend) -> &'static dyn ZoneBackend {
    match kind {
        Backend::Container => &ContainerBackend,
        Backend::QemuMicrovm => &QemuMicrovmBackend,
        Backend::Libvirt => &LibvirtBackend,
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
        resolved = NodePlan {
            bundle: Some(pulled_dir),
            remote_bundle: None,
            ..np.clone()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_backend_kind_selects_the_implementation_it_names() {
        assert_eq!(backend_for(Backend::Container).name(), "container");
        assert_eq!(backend_for(Backend::QemuMicrovm).name(), "qemu-microvm");
        assert_eq!(backend_for(Backend::Libvirt).name(), "libvirt");
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
}
