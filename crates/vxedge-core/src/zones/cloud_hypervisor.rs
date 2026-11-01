//! `cloud-hypervisor` zone backend (Track V4): the same direct-kernel guest bundle as
//! `qemu-microvm`, driven through Cloud Hypervisor's REST API on a per-node Unix socket rather
//! than through a command line.
//!
//! The VMM is started empty and told what to run over the API, so [`vm_config`] is the whole
//! description of a guest and is pure. It runs as vxedged's own user for the reason the QEMU
//! launch gives: `/dev/kvm` comes from the `kvm` group, the tap's owner is set when the gateway
//! creates it, and a root-owned socket could not be read back.
//!
//! **Liveness comes from the socket, not a pidfile.** A pid can be reused and a `kill -0` says
//! nothing about the guest. `vmm.ping` answers only from a live VMM and reports its pid, and
//! `vm.info` says whether the guest runs. The VMM does not daemonize, so `setsid -f` detaches
//! it: it must outlive the caller and must not linger as this process's zombie.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use cloud_hypervisor_client::apis::DefaultApi;
use cloud_hypervisor_client::models::{
    ConsoleConfig, ConsoleMode, CpusConfig, MemoryConfig, NetConfig, PayloadConfig, SerialConfig,
    VmConfig, VmState,
};
use cloud_hypervisor_client::{SocketBasedApiClient, socket_based_api_client};

use super::backend::{ZoneBackendError, block_on, bundle_paths, kernel_cmdline, wait_until};
use super::plan::NodePlan;
use crate::gateway::Shell;
use crate::gateway::netops::children_are_privileged;
use crate::zones::backend::NodeState;

const VMM: &str = "cloud-hypervisor";
/// The same allocation `qemu-microvm` gives a guest, so the two backends compare directly.
const VCPUS: i32 = 1;
const MEMORY_BYTES: i64 = 128 * 1024 * 1024;
/// How long one API call may take. A VMM that stopped answering must not hang `zone status`.
const API_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a new VMM gets to open its socket, and a stopped one to exit.
const VMM_TIMEOUT: Duration = Duration::from_secs(5);
/// `sockaddr_un.sun_path` is 108 bytes including the terminating NUL.
const SOCKET_PATH_MAX: usize = 107;

fn socket_path(np: &NodePlan, runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(format!("{}.ch.sock", np.runtime_name()))
}

fn serial_path(np: &NodePlan, runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(format!("{}.serial.log", np.runtime_name()))
}

fn log_path(np: &NodePlan, runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(format!("{}.ch.log", np.runtime_name()))
}

fn err(np: &NodePlan, detail: String) -> ZoneBackendError {
    ZoneBackendError::Runtime {
        node: np.node_id.clone(),
        detail,
    }
}

/// A stable, locally administered unicast MAC for a node. Left unset, the VMM draws a random
/// one per boot, and a run should not differ from the last in anything the manifest fixed.
fn guest_mac(np: &NodePlan) -> String {
    // FNV-1a: a stable function of the id, unlike the standard library's hasher.
    let hash = np.node_id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    let b = hash.to_be_bytes();
    format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b[0], b[1], b[2], b[3], b[4]
    )
}

/// The guest, as the API takes it.
///
/// The tap is named and `ip`/`mask` stay unset: the gateway owns that port, and it is a bridge
/// member that must carry no host address.
fn vm_config(np: &NodePlan, kernel: String, initrd: String, serial: &Path) -> VmConfig {
    let mut payload = PayloadConfig::new();
    payload.kernel = Some(kernel);
    payload.initramfs = Some(initrd);
    payload.cmdline = Some(kernel_cmdline(np));

    let mut net = NetConfig::new();
    net.tap = Some(np.endpoint.clone());
    net.mac = Some(guest_mac(np));

    let mut serial_console = SerialConfig::new(ConsoleMode::File);
    serial_console.file = Some(serial.display().to_string());

    let mut config = VmConfig::new(payload);
    config.cpus = Some(CpusConfig::new(VCPUS, VCPUS));
    config.memory = Some(MemoryConfig::new(MEMORY_BYTES));
    config.net = Some(vec![net]);
    config.serial = Some(serial_console);
    config.console = Some(ConsoleConfig::new(ConsoleMode::Off));
    config
}

/// The VMM's command line: the socket and nothing else, since the guest arrives over the API.
///
/// `taskset` wraps the whole process rather than using `CpusConfig::affinity`, which pins vCPU
/// threads only. A pinned cell keeps every thread of its VM on its own CPUs, as `qemu-microvm`
/// does, virtio workers included.
fn launch_argv(np: &NodePlan, socket: &Path, netns: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    if let Some(ns) = netns {
        argv.extend(["ip", "netns", "exec", ns].map(String::from));
    }
    argv.extend(["setsid", "-f"].map(String::from));
    if !np.pinned_cpus.is_empty() {
        let cpus: Vec<String> = np.pinned_cpus.iter().map(u32::to_string).collect();
        argv.extend(["taskset".to_string(), "-c".to_string(), cpus.join(",")]);
    }
    argv.push(VMM.to_string());
    argv.push("--api-socket".to_string());
    argv.push(format!("path={}", socket.display()));
    argv
}

/// A netns entered through `sudo` would give the VMM a root-owned socket this process cannot
/// connect to, so that combination is refused rather than started and lost.
fn refuse_escalated_netns(np: &NodePlan, sh: &Shell) -> Result<(), ZoneBackendError> {
    if sh.netns.is_some() && sh.escalate && !children_are_privileged() {
        return Err(err(
            np,
            format!(
                "`{VMM}` cannot start inside a network namespace through `sudo`: its API socket \
                 would be root-owned and unreachable from this process. Run the gateway in the \
                 host namespace, or run this whole process as root."
            ),
        ));
    }
    Ok(())
}

fn refuse_long_socket_path(np: &NodePlan, socket: &Path) -> Result<(), ZoneBackendError> {
    let len = socket.as_os_str().len();
    if len > SOCKET_PATH_MAX {
        return Err(err(
            np,
            format!(
                "API socket path `{}` is {len} bytes, and Unix sockets allow {SOCKET_PATH_MAX}. \
                 Use a shorter `--runtime-dir`.",
                socket.display()
            ),
        ));
    }
    Ok(())
}

fn vmm_on_path() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(VMM).is_file()))
}

/// What is on the far side of a node's socket.
#[derive(Debug, PartialEq)]
enum Vmm {
    /// No socket file.
    Absent,
    /// A socket file nothing answers on: a VMM that died without cleaning up.
    Stale,
    /// A VMM answered. `running` says whether its guest does.
    Alive { pid: Option<i64>, running: bool },
}

async fn timed<T, E>(f: impl Future<Output = Result<T, E>>) -> Option<Result<T, E>> {
    tokio::time::timeout(API_TIMEOUT, f).await.ok()
}

fn client(socket: &Path) -> SocketBasedApiClient {
    socket_based_api_client(socket)
}

fn probe(socket: &Path) -> Vmm {
    if !socket.exists() {
        return Vmm::Absent;
    }
    let c = client(socket);
    block_on(async {
        let Some(Ok(ping)) = timed(c.vmm_ping_get()).await else {
            return Vmm::Stale;
        };
        // A VMM with no VM created answers 404 here, which is simply "not running".
        let running = matches!(
            timed(c.vm_info_get()).await,
            Some(Ok(info)) if info.state == VmState::Running
        );
        Vmm::Alive {
            pid: ping.pid,
            running,
        }
    })
}

/// Asks the VMM to exit, then makes sure it did and takes its socket away.
fn stop(np: &NodePlan, socket: &Path, pid: Option<i64>) -> Result<(), ZoneBackendError> {
    let c = client(socket);
    let _ = block_on(timed(c.shutdown_vmm()));
    let gone = |pid: i64| !Path::new(&format!("/proc/{pid}")).exists();
    if let Some(pid) = pid
        && !wait_until(VMM_TIMEOUT, || gone(pid))
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        if !wait_until(VMM_TIMEOUT, || gone(pid)) {
            return Err(err(
                np,
                format!(
                    "`{VMM}` (pid {pid}) did not exit after SIGKILL. Check it with `ps -o \
                     stat,cmd -p {pid}`; a process stuck in `D` state is waiting on the kernel."
                ),
            ));
        }
    }
    remove_socket(np, socket)
}

fn remove_socket(np: &NodePlan, socket: &Path) -> Result<(), ZoneBackendError> {
    match std::fs::remove_file(socket) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(err(
            np,
            format!(
                "cannot remove API socket `{}`: {e}. Remove it by hand and retry.",
                socket.display()
            ),
        )),
        _ => Ok(()),
    }
}

fn launch(np: &NodePlan, sh: &Shell, socket: &Path, log: &Path) -> Result<(), ZoneBackendError> {
    let argv = launch_argv(np, socket, sh.netns.as_deref());
    let open_log = || {
        std::fs::File::options()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| {
                err(
                    np,
                    format!(
                        "cannot open `{}` for the VMM's log: {e}. Check `--runtime-dir` is \
                         writable by this user.",
                        log.display()
                    ),
                )
            })
    };
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(open_log()?)
        .stderr(open_log()?)
        .status()
        .map_err(|e| err(np, format!("cannot run `{}`: {e}", argv[0])))?;
    if !status.success() {
        return Err(err(
            np,
            format!(
                "`{}` exited with {status} before `{VMM}` started. See `{}`.",
                argv.join(" "),
                log.display()
            ),
        ));
    }
    Ok(())
}

pub(super) fn up(sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
    let socket = socket_path(np, runtime_dir);
    match probe(&socket) {
        Vmm::Alive { running: true, .. } => return Ok(()), // idempotent
        // Whatever left a VMM without a running guest, a fresh one is the one way to start.
        Vmm::Alive { pid, .. } => stop(np, &socket, pid)?,
        Vmm::Stale => remove_socket(np, &socket)?,
        Vmm::Absent => {}
    }
    refuse_escalated_netns(np, sh)?;
    refuse_long_socket_path(np, &socket)?;
    if !vmm_on_path() {
        return Err(err(
            np,
            format!(
                "`{VMM}` is not on PATH. Install a release from \
                 https://github.com/cloud-hypervisor/cloud-hypervisor/releases, or use `backend: \
                 qemu-microvm` for this device."
            ),
        ));
    }
    let (kernel, initrd) = bundle_paths(np)?;
    let config = vm_config(np, kernel, initrd, &serial_path(np, runtime_dir));

    let log = log_path(np, runtime_dir);
    launch(np, sh, &socket, &log)?;
    if !wait_until(VMM_TIMEOUT, || matches!(probe(&socket), Vmm::Alive { .. })) {
        return Err(err(
            np,
            format!(
                "`{VMM}` did not answer on `{}` within {} s. See `{}`.",
                socket.display(),
                VMM_TIMEOUT.as_secs(),
                log.display()
            ),
        ));
    }

    let c = client(&socket);
    let booted = block_on(async {
        c.create_vm(config).await?;
        c.boot_vm().await
    });
    if let Err(e) = booted {
        let pid = match probe(&socket) {
            Vmm::Alive { pid, .. } => pid,
            _ => None,
        };
        stop(np, &socket, pid)?;
        return Err(err(
            np,
            format!(
                "`{VMM}` refused the guest: {e}. If it names the tap, check `{}` exists and \
                 belongs to this user (`ip -d link show`, `vxedge up` creates it). See `{}`.",
                np.endpoint,
                log.display()
            ),
        ));
    }
    Ok(())
}

pub(super) fn down(np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
    let socket = socket_path(np, runtime_dir);
    match probe(&socket) {
        Vmm::Alive { pid, .. } => stop(np, &socket, pid),
        Vmm::Stale => remove_socket(np, &socket),
        Vmm::Absent => Ok(()),
    }
}

pub(super) fn status(np: &NodePlan, runtime_dir: &Path) -> NodeState {
    match probe(&socket_path(np, runtime_dir)) {
        Vmm::Alive { running: true, .. } => NodeState::Up,
        _ => NodeState::Down,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::plan::EndpointKind;
    use crate::model::{Backend, DeviceRole};

    fn node(pinned_cpus: Vec<u32>) -> NodePlan {
        NodePlan {
            zone_id: "z".into(),
            node_id: "node-ch-01".into(),
            role: DeviceRole::Ecu,
            backend: Backend::CloudHypervisor,
            endpoint: "vxtap0".into(),
            peer: "vxtap0".into(),
            kind: EndpointKind::Tap,
            vlan: 101,
            slice: Some("adas".into()),
            ip_cidr: Some("10.101.0.2/24".into()),
            bundle: None,
            remote_bundle: None,
            image: None,
            pinned_cpus,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vx-ch-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_guest_adopts_the_gateway_tap_and_configures_no_host_address() {
        let c = vm_config(
            &node(vec![]),
            "/b/vmlinuz".into(),
            "/b/initrd.img".into(),
            Path::new("/r/serial.log"),
        );
        let net = c.net.as_ref().unwrap();
        assert_eq!(net.len(), 1);
        assert_eq!(net[0].tap.as_deref(), Some("vxtap0"));
        assert_eq!(net[0].ip, None);
        assert_eq!(net[0].mask, None);
    }

    #[test]
    fn a_node_keeps_its_mac_across_boots_and_no_two_nodes_share_one() {
        let a = node(vec![]);
        let mut b = node(vec![]);
        b.node_id = "node-ch-02".into();
        assert_eq!(guest_mac(&a), guest_mac(&a));
        assert_ne!(guest_mac(&a), guest_mac(&b));
        let first = u8::from_str_radix(&guest_mac(&a)[..2], 16).unwrap();
        assert_eq!(first & 0b11, 0b10, "locally administered and unicast");
    }

    #[test]
    fn the_guest_boots_the_bundle_with_the_contract_cmdline_and_a_serial_file() {
        let c = vm_config(
            &node(vec![]),
            "/b/vmlinuz".into(),
            "/b/initrd.img".into(),
            Path::new("/r/serial.log"),
        );
        assert_eq!(c.payload.kernel.as_deref(), Some("/b/vmlinuz"));
        assert_eq!(c.payload.initramfs.as_deref(), Some("/b/initrd.img"));
        let cmdline = c.payload.cmdline.as_deref().unwrap();
        assert!(cmdline.contains("console=ttyS0"), "{cmdline}");
        assert!(cmdline.contains("vx.ip=10.101.0.2/24"), "{cmdline}");
        let serial = c.serial.as_ref().unwrap();
        assert_eq!(serial.mode, ConsoleMode::File);
        assert_eq!(serial.file.as_deref(), Some("/r/serial.log"));
    }

    #[test]
    fn the_vmm_starts_detached_with_only_its_socket() {
        let argv = launch_argv(&node(vec![]), Path::new("/r/n.sock"), None);
        assert_eq!(
            argv,
            [
                "setsid",
                "-f",
                "cloud-hypervisor",
                "--api-socket",
                "path=/r/n.sock"
            ]
        );
    }

    #[test]
    fn a_pinned_node_and_a_netns_wrap_the_vmm_in_the_right_order() {
        let argv = launch_argv(&node(vec![2, 3]), Path::new("/s"), Some("ns1"));
        assert_eq!(
            argv[..9],
            [
                "ip", "netns", "exec", "ns1", "setsid", "-f", "taskset", "-c", "2,3"
            ]
        );
        assert_eq!(argv[9], "cloud-hypervisor");
    }

    #[test]
    fn a_socket_path_unix_cannot_bind_is_refused_with_its_fix() {
        let long = PathBuf::from(format!("/{}", "r".repeat(SOCKET_PATH_MAX)));
        let e = refuse_long_socket_path(&node(vec![]), &long).unwrap_err();
        assert!(e.to_string().contains("--runtime-dir"), "{e}");
        assert!(refuse_long_socket_path(&node(vec![]), Path::new("/r/n.sock")).is_ok());
    }

    #[test]
    fn a_node_with_no_socket_is_down_and_stopping_it_is_a_no_op() {
        let dir = scratch("absent");
        let n = node(vec![]);
        assert_eq!(status(&n, &dir), NodeState::Down);
        down(&n, &dir).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A VMM killed outright leaves its socket file behind. Nothing answers on it, so the node
    /// is down, and `down` clears the file so a later `up` can bind the path again.
    #[test]
    fn a_socket_nobody_answers_is_down_and_down_removes_it() {
        let dir = scratch("stale");
        let n = node(vec![]);
        let sock = socket_path(&n, &dir);
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        assert!(sock.exists(), "binding leaves the file behind");

        assert_eq!(probe(&sock), Vmm::Stale);
        assert_eq!(status(&n, &dir), NodeState::Down);
        down(&n, &dir).unwrap();
        assert!(!sock.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
