//! `network.time_sync` (`kind: gptp`, `ARCHITECTURE.md` §6.0.3): the gateway is the grandmaster
//! of an IEEE 802.1AS domain over its followers' access ports, and each follower measures its
//! offset from the grandmaster and the peer delay.
//!
//! Every `ptp4l` here runs the vxparts `linuxptp` part, [`LINUXPTP_IMAGE`], with linuxptp's
//! 802.1AS profile on software timestamps.
//!
//! # The grandmaster
//!
//! Its master ports are the followers' access ports: the bridge's own ports, in the network
//! namespace the bridge lives in. gPTP frames go to `01:80:C2:00:00:0E`, which a Linux bridge
//! passes up on the receiving port and does not forward, so a `ptp4l` on the port hears that
//! follower only. Opening a raw socket on a port needs `CAP_NET_RAW` in the user namespace that
//! owns the bridge's network namespace, which a rootless container does not have: run with
//! `--network host` it fails with `socket failed: Operation not permitted`. So the image runs
//! rootless as a `hold` container (the group [`grandmaster_group`]), and the grandmaster is
//! started from that container's file system as root, in the bridge's namespace:
//! `sudo -n nsenter --target <hold pid> --mount --pid -- gptp gateway …`
//! ([`grandmaster_command`]). That is the `sudo -n nsenter` the 5G core already takes, scoped
//! the same way: `podman` never escalates.
//!
//! Entering the hold container's PID namespace is what tears it down: `ptp4l` is a member of it,
//! so removing the container kills it, and vxedged, which may not signal a root process, never
//! has to. Its management socket lives in a host directory under the run's runtime directory,
//! mounted into the container at the same path. In the container's own file system a root
//! `ptp4l` cannot create one (`bind` fails with `EOVERFLOW`: root is not mapped in the
//! container's user namespace).
//!
//! # Followers
//!
//! A follower's `ptp4l` is one more member of its device's container group, as a placed service
//! is (§6.0.1), on the device's `eth0`. It runs free (`free_running 1`): containers share the
//! host's clock, so it measures the time-sync path and does not steer the clock.
//!
//! # Reading a follower
//!
//! [`read_follower`] asks a follower's `ptp4l` over its management socket, as `pmc` does:
//! `CURRENT_DATA_SET` for `offsetFromMaster` and `PORT_DATA_SET` for `peerMeanPathDelay` (with
//! the peer-to-peer delay mechanism `CURRENT_DATA_SET`'s `meanPathDelay` stays 0). The socket is
//! in a host directory mounted into the follower at the same path, so vxedged reaches it
//! without entering any namespace or starting a process per scrape.
//!
//! # Names
//!
//! Every host-global name comes from the run: the sockets, logs and pidfile live under the
//! runtime directory, the grandmaster's group is named after the gateway's bridge, and a
//! follower's directory after its node ([`crate::zones::runtime_name_of`]). Nothing falls back
//! to linuxptp's defaults (`/var/run/ptp4l`), so two runs on one host cannot collide.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::containers::{Bind, ContainerClient, ContainerSpec, GroupSpec, InfraNetwork};
use crate::gateway::{GatewayPlan, Shell};

/// The vxparts `linuxptp` part, 0.1.0 (linuxptp 4.2), pinned by its registry digest.
pub const LINUXPTP_IMAGE: &str = "ghcr.io/vxedge/parts/agent/time/app/linuxptp:0.1.0@sha256:cc81350e76480ee37237abcc78f42871d97c71ed1cd35e431a02924a521cc94b";

/// A follower's `ptp4l` in its device's container group.
pub const FOLLOWER_MEMBER: &str = "gptp";

/// The image's entrypoint, which the grandmaster is started with from outside the container.
const ENTRYPOINT: &str = "/usr/local/bin/gptp";

/// The interface a container node's gateway port has in its namespace
/// (`zones::backend`'s container backend renames it so).
const NODE_PORT: &str = "eth0";

/// The longest socket path a Unix socket address holds (`sun_path` is 108 bytes), less room for
/// the `.ro` socket and a reader's own socket beside it.
const MAX_SOCKET_PATH: usize = 96;

/// How long a follower has to answer one management request.
const ANSWER_WITHIN: Duration = Duration::from_millis(200);

#[derive(Debug, thiserror::Error)]
pub enum GptpError {
    #[error("gPTP state directory `{path}`: {detail}")]
    Dir { path: PathBuf, detail: String },
    #[error(
        "gPTP socket path `{path}` is {len} bytes, longer than the {MAX_SOCKET_PATH} a Unix \
         socket address leaves room for. Use a shorter runtime directory."
    )]
    SocketPathTooLong { path: PathBuf, len: usize },
    #[error("gPTP grandmaster container: {0}")]
    Container(#[from] crate::containers::ContainerError),
    #[error("gPTP grandmaster: {0}")]
    Name(#[from] crate::containers::NameError),
    #[error("cannot start the gPTP grandmaster `{program}`: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("the gPTP grandmaster exited at start: {log}")]
    Exited { log: String },
}

/// The images a run of `exp` starts for its time sync: [`LINUXPTP_IMAGE`] when it declares
/// `network.time_sync`, none otherwise. The package lock pins it and an archive records its
/// digest, as for the 5G core's images.
pub fn gptp_images(exp: &crate::model::Experiment) -> Vec<&'static str> {
    if exp.time_sync.is_some() {
        vec![LINUXPTP_IMAGE]
    } else {
        Vec::new()
    }
}

/// This run's gPTP directory for `name`: sockets, log and pidfile.
fn dir(runtime_dir: &Path, name: &str) -> PathBuf {
    runtime_dir.join("gptp").join(name)
}

/// The grandmaster's directory: named after the gateway's bridge.
pub fn grandmaster_dir(runtime_dir: &Path, bridge: &str) -> PathBuf {
    dir(runtime_dir, &format!("gm-{bridge}"))
}

/// A follower's directory: named after its node's runtime name, so after its slot too.
pub fn follower_dir(
    runtime_dir: &Path,
    instance: crate::instance::Instance,
    device: &str,
) -> PathBuf {
    dir(
        runtime_dir,
        &crate::zones::runtime_name_of(instance, device),
    )
}

/// The management socket of the `ptp4l` whose directory is `dir`. The read-only one is
/// `<socket>.ro`; the image's `gptp` script sets both.
pub fn socket(dir: &Path) -> PathBuf {
    dir.join("ptp4l")
}

/// The grandmaster's container group, named after the gateway's bridge.
pub fn grandmaster_group_name(bridge: &str) -> String {
    format!("vxgptp-{bridge}")
}

/// Create `dir` and check its socket path fits a Unix socket address.
pub fn prepare_dir(dir: &Path) -> Result<(), GptpError> {
    let path = socket(dir);
    let len = path.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(GptpError::SocketPathTooLong { path, len });
    }
    std::fs::create_dir_all(dir).map_err(|e| GptpError::Dir {
        path: dir.to_path_buf(),
        detail: e.to_string(),
    })
}

/// A host directory mounted at the same path, so a socket path means the same file on both
/// sides: `ptp4l` answers a reader at the path the reader bound.
fn same_path(dir: &Path) -> Bind {
    Bind {
        host: dir.to_path_buf(),
        container: dir.display().to_string(),
        read_only: false,
        relabel: false,
    }
}

/// The grandmaster's container group: the image in `hold`, with no network of its own and its
/// directory mounted. It holds the image's file system and a PID namespace for
/// [`grandmaster_command`].
pub fn grandmaster_group(
    image: &str,
    bridge: &str,
    runtime_dir: &Path,
) -> Result<GroupSpec, crate::containers::NameError> {
    let infra = ContainerSpec {
        image: image.to_string(),
        cmd: vec!["hold".into()],
        binds: vec![same_path(&grandmaster_dir(runtime_dir, bridge))],
        ..Default::default()
    };
    Ok(GroupSpec::new(grandmaster_group_name(bridge), infra)?
        .with_infra_network(InfraNetwork::None))
}

/// The command that starts the grandmaster: the image's `gptp gateway`, run as root from the
/// hold container's mount and PID namespaces, in the network namespace the bridge lives in (the
/// caller's, or `netns`). `escalate` prefixes `sudo -n`. One `--uds` from the run's directory,
/// which the script passes as both `--uds_address` and `--uds_ro_address`. Pure.
pub fn grandmaster_command(
    escalate: bool,
    netns: Option<&str>,
    hold_pid: u32,
    socket: &Path,
    ports: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    if escalate {
        argv.extend(["sudo".into(), "-n".into()]);
    }
    argv.extend([
        "nsenter".into(),
        "--target".into(),
        hold_pid.to_string(),
        "--mount".into(),
        "--pid".into(),
    ]);
    if let Some(ns) = netns {
        argv.push(format!(
            "--net={}",
            crate::gateway::netlink::netns_path(ns).display()
        ));
    }
    argv.extend([
        "--".into(),
        ENTRYPOINT.into(),
        "gateway".into(),
        "--uds".into(),
        socket.display().to_string(),
    ]);
    argv.extend(ports.iter().cloned());
    argv
}

/// Whether `sh`'s commands are prefixed with `sudo -n`: the rule `Shell` itself applies.
fn escalates(sh: &Shell) -> bool {
    sh.escalate && !crate::gateway::netops::children_are_privileged()
}

/// Whether this host can start the grandmaster: its commands need no `sudo`, or `sudo -n`
/// allows `nsenter` without a password (a NOPASSWD rule, or a credential cached now).
pub fn can_run_grandmaster(sh: &Shell) -> bool {
    if !escalates(sh) {
        return true;
    }
    let Some(nsenter) = which("nsenter") else {
        return false;
    };
    std::process::Command::new("sudo")
        .arg("-n")
        .arg("-l")
        .arg(&nsenter)
        .stdin(std::process::Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success())
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .chain(["/usr/sbin", "/usr/bin", "/sbin", "/bin"])
        .map(|d| Path::new(d).join(program))
        .find(|p| p.is_file())
}

fn pidfile(dir: &Path) -> PathBuf {
    dir.join("grandmaster.pid")
}

fn cmdfile(dir: &Path) -> PathBuf {
    dir.join("grandmaster.cmd")
}

/// The grandmaster's log: `ptp4l`'s output, under the run's runtime directory.
pub fn grandmaster_log(runtime_dir: &Path, bridge: &str) -> PathBuf {
    grandmaster_dir(runtime_dir, bridge).join("grandmaster.log")
}

/// Whether the process in `dir`'s pidfile is still the grandmaster's `nsenter`. Read from
/// `/proc/<pid>/cmdline`, which anyone may read: `kill -0` on a root process answers EPERM.
fn grandmaster_pid_alive(dir: &Path) -> bool {
    let Some(pid) = std::fs::read_to_string(pidfile(dir))
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
    else {
        return false;
    };
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
        let c = String::from_utf8_lossy(&c);
        c.contains("nsenter") && c.contains("gateway")
    })
}

/// Whether the gateway's grandmaster is running.
pub fn grandmaster_running(runtime_dir: &Path, bridge: &str) -> bool {
    grandmaster_pid_alive(&grandmaster_dir(runtime_dir, bridge))
}

/// Bring the grandmaster of `plan`'s `time_sync` up, after the gateway's access ports exist.
/// Idempotent: a running grandmaster started with the same command is left alone; one started
/// for other ports is replaced, by recreating its container. A plan without `time_sync` does
/// nothing.
pub async fn grandmaster_up(
    client: &ContainerClient,
    sh: &Shell,
    plan: &GatewayPlan,
    runtime_dir: &Path,
    image: &str,
) -> Result<(), GptpError> {
    let Some(ts) = &plan.time_sync else {
        return Ok(());
    };
    let dir = grandmaster_dir(runtime_dir, &plan.bridge);
    prepare_dir(&dir)?;
    client.ensure_image(image).await?;
    let group = grandmaster_group(image, &plan.bridge, runtime_dir)?;
    let (mut handle, _) = client.ensure_group(&group).await?;
    let mut argv = grandmaster_command(
        escalates(sh),
        sh.netns.as_deref(),
        handle.infra_pid,
        &socket(&dir),
        &ts.master_ports(),
    );
    if grandmaster_pid_alive(&dir) {
        if std::fs::read_to_string(cmdfile(&dir)).ok() == Some(argv.join(" ")) {
            return Ok(());
        }
        // Another command, such as other ports: the old `ptp4l` goes with its container.
        client.group_down(group.name()).await?;
        wait_gone(&dir).await;
        (handle, _) = client.ensure_group(&group).await?;
        argv = grandmaster_command(
            escalates(sh),
            sh.netns.as_deref(),
            handle.infra_pid,
            &socket(&dir),
            &ts.master_ports(),
        );
    }
    let log_path = dir.join("grandmaster.log");
    let log = std::fs::File::create(&log_path).map_err(|e| GptpError::Dir {
        path: log_path.clone(),
        detail: e.to_string(),
    })?;
    let log_err = log.try_clone().map_err(|e| GptpError::Dir {
        path: log_path.clone(),
        detail: e.to_string(),
    })?;
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .spawn()
        .map_err(|source| GptpError::Spawn {
            program: argv[0].clone(),
            source,
        })?;
    let write = |path: PathBuf, text: String| {
        std::fs::write(&path, text).map_err(|e| GptpError::Dir {
            path,
            detail: e.to_string(),
        })
    };
    write(pidfile(&dir), child.id().to_string())?;
    write(cmdfile(&dir), argv.join(" "))?;
    // Confirm it stays up: `sudo -n` without a rule, or a port that does not exist, ends it at
    // once. Then reap it whenever it exits, so its pid never lingers as a zombie.
    tokio::time::sleep(Duration::from_secs(1)).await;
    if let Ok(Some(_)) = child.try_wait() {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let _ = std::fs::remove_file(pidfile(&dir));
        let _ = client.group_down(group.name()).await;
        return Err(GptpError::Exited {
            log: log.lines().rev().take(5).collect::<Vec<_>>().join(" | "),
        });
    }
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Wait up to 5 s for the grandmaster's `nsenter` to go after its container did.
async fn wait_gone(dir: &Path) {
    for _ in 0..50 {
        if !grandmaster_pid_alive(dir) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Stop the grandmaster of the gateway on `bridge`: removing its container ends its PID
/// namespace, and `ptp4l` with it. Idempotent.
pub async fn grandmaster_down(
    client: &ContainerClient,
    bridge: &str,
    runtime_dir: &Path,
) -> Result<(), GptpError> {
    client.group_down(&grandmaster_group_name(bridge)).await?;
    let dir = grandmaster_dir(runtime_dir, bridge);
    wait_gone(&dir).await;
    // Its sockets are root's, in this run's directory, so they are removed here. The log stays.
    let sock = socket(&dir);
    let mut ro = sock.clone().into_os_string();
    ro.push(".ro");
    for path in [pidfile(&dir), cmdfile(&dir), sock, PathBuf::from(ro)] {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// The container a follower's `ptp4l` runs as, in its device's group: free-running, on the
/// node's port, its management socket in [`follower_dir`] mounted at the same path. `NET_RAW`
/// for its raw socket, which Podman's default capabilities lack.
pub fn follower_spec(
    image: &str,
    runtime_dir: &Path,
    instance: crate::instance::Instance,
    device: &str,
) -> ContainerSpec {
    let dir = follower_dir(runtime_dir, instance, device);
    ContainerSpec {
        name: FOLLOWER_MEMBER.to_string(),
        image: image.to_string(),
        cmd: vec![
            "follower".into(),
            "--uds".into(),
            socket(&dir).display().to_string(),
            NODE_PORT.into(),
        ],
        binds: vec![same_path(&dir)],
        cap_add: vec!["NET_RAW".into()],
        ..Default::default()
    }
}

/// What a follower's `ptp4l` reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// `CURRENT_DATA_SET.offsetFromMaster`, in seconds.
    pub offset_s: f64,
    /// `PORT_DATA_SET.peerMeanPathDelay`, in seconds.
    pub path_delay_s: f64,
}

/// Ask the follower whose directory is `dir` for its offset and peer delay. `None` when it does
/// not answer, or answers without a grandmaster to follow: its port is not `UNCALIBRATED` (where
/// a free-running follower stays) or `SLAVE`, so its offset would describe no grandmaster.
pub fn read_follower(dir: &Path) -> Option<Reading> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let local = dir.join(format!("q-{}-{n}", std::process::id()));
    let _ = std::fs::remove_file(&local);
    let sock = std::os::unix::net::UnixDatagram::bind(&local).ok();
    let read = sock.and_then(|sock| {
        sock.set_read_timeout(Some(ANSWER_WITHIN)).ok()?;
        let target = socket(dir);
        let current = mgmt::ask(&sock, &target, mgmt::CURRENT_DATA_SET, (n as u16) << 1)?;
        let port = mgmt::ask(&sock, &target, mgmt::PORT_DATA_SET, ((n as u16) << 1) | 1)?;
        let offset = mgmt::current_offset(&current)?;
        let (state, delay) = mgmt::port_state_and_delay(&port)?;
        matches!(state, mgmt::UNCALIBRATED | mgmt::SLAVE).then_some(Reading {
            offset_s: offset,
            path_delay_s: delay,
        })
    });
    let _ = std::fs::remove_file(&local);
    read
}

/// `vxedge_gptp_offset_seconds` and `vxedge_gptp_path_delay_seconds` (§12, host-side
/// measurements) for each follower of `exp` that answers, with the follower as entity, as
/// Prometheus text. A follower that does not answer has no sample: the run's samples show a gap.
pub fn metrics(
    exp: &crate::model::Experiment,
    runtime_dir: &Path,
    instance: crate::instance::Instance,
) -> String {
    let Some(ts) = &exp.time_sync else {
        return String::new();
    };
    let readings: Vec<(String, Reading)> = ts
        .followers
        .iter()
        .filter_map(|device| {
            let entity = crate::entities::node_path(exp, device)?;
            Some((
                entity,
                read_follower(&follower_dir(runtime_dir, instance, device))?,
            ))
        })
        .collect();
    render(&readings)
}

/// One series family: name, help, and the reading it serves.
type Family = (&'static str, &'static str, fn(&Reading) -> f64);

/// [`metrics`]' text for these readings. A family with no samples is left out. Pure.
pub fn render(readings: &[(String, Reading)]) -> String {
    use std::fmt::Write as _;
    if readings.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let families: [Family; 2] = [
        (
            "vxedge_gptp_offset_seconds",
            "The follower's offset from the gPTP grandmaster (offsetFromMaster), read from its ptp4l.",
            |r| r.offset_s,
        ),
        (
            "vxedge_gptp_path_delay_seconds",
            "The follower's peer delay to the gateway (peerMeanPathDelay), read from its ptp4l.",
            |r| r.path_delay_s,
        ),
    ];
    for (name, help, value) in families {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        for (entity, r) in readings {
            let entity = entity.replace('\\', "\\\\").replace('"', "\\\"");
            let _ = writeln!(out, "{name}{{entity=\"{entity}\"}} {}", value(r));
        }
    }
    out
}

/// IEEE 1588 management messages, as much of them as reading a follower takes: a `GET` and the
/// `RESPONSE` to it, over `ptp4l`'s Unix socket. Pure apart from [`ask`].
mod mgmt {
    use std::path::Path;

    pub const CURRENT_DATA_SET: u16 = 0x2001;
    pub const PORT_DATA_SET: u16 = 0x2004;
    pub const UNCALIBRATED: u8 = 8;
    pub const SLAVE: u8 = 9;

    /// The profile's `transportSpecific` (gPTP.cfg's `0x1`). `ptp4l` ignores a management
    /// request that carries another, `pmc`'s default 0 included.
    const TRANSPORT_SPECIFIC: u8 = 0x1;
    const MANAGEMENT: u8 = 0xD;
    const HEADER: usize = 34;
    /// targetPortIdentity, startingBoundaryHops, boundaryHops, actionField, reserved.
    const BODY: usize = 14;
    const TLV_MANAGEMENT: u16 = 0x0001;
    const GET: u8 = 0;
    const RESPONSE: u8 = 2;

    /// A `GET` of `id` to every port of the clock (the wildcard target `pmc` uses).
    pub fn get(id: u16, sequence: u16) -> Vec<u8> {
        let len = HEADER + BODY + 6;
        let mut m = vec![0u8; len];
        m[0] = (TRANSPORT_SPECIFIC << 4) | MANAGEMENT;
        m[1] = 2; // versionPTP
        m[2..4].copy_from_slice(&(len as u16).to_be_bytes());
        // sourcePortIdentity: any identity; the answer goes to the socket that asked.
        m[20..28].copy_from_slice(&(std::process::id() as u64).to_be_bytes());
        m[30..32].copy_from_slice(&sequence.to_be_bytes());
        m[32] = 0x04; // controlField: management
        m[33] = 0x7f; // logMessageInterval
        m[HEADER..HEADER + 10].fill(0xff); // targetPortIdentity: all clocks, all ports
        m[HEADER + 12] = GET;
        let tlv = HEADER + BODY;
        m[tlv..tlv + 2].copy_from_slice(&TLV_MANAGEMENT.to_be_bytes());
        m[tlv + 2..tlv + 4].copy_from_slice(&2u16.to_be_bytes());
        m[tlv + 4..tlv + 6].copy_from_slice(&id.to_be_bytes());
        m
    }

    /// The data of a `RESPONSE` to `GET id` with `sequence`, or `None` for anything else (an
    /// error status, another request's answer, a short message).
    pub fn response_data(msg: &[u8], id: u16, sequence: u16) -> Option<&[u8]> {
        if msg.len() < HEADER + BODY + 6
            || msg[0] & 0x0f != MANAGEMENT
            || u16::from_be_bytes([msg[30], msg[31]]) != sequence
            || msg[HEADER + 12] & 0x0f != RESPONSE
        {
            return None;
        }
        let tlv = HEADER + BODY;
        let kind = u16::from_be_bytes([msg[tlv], msg[tlv + 1]]);
        let length = usize::from(u16::from_be_bytes([msg[tlv + 2], msg[tlv + 3]]));
        let mid = u16::from_be_bytes([msg[tlv + 4], msg[tlv + 5]]);
        if kind != TLV_MANAGEMENT || mid != id || length < 2 {
            return None;
        }
        msg.get(tlv + 6..tlv + 4 + length)
    }

    /// A `TimeInterval`: nanoseconds scaled by 2^16, as seconds.
    fn time_interval(b: &[u8]) -> Option<f64> {
        let raw = i64::from_be_bytes(b.get(..8)?.try_into().ok()?);
        Some(raw as f64 / 65536.0 / 1e9)
    }

    /// `CURRENT_DATA_SET`: stepsRemoved, offsetFromMaster, meanPathDelay.
    pub fn current_offset(data: &[u8]) -> Option<f64> {
        time_interval(data.get(2..10)?)
    }

    /// `PORT_DATA_SET`: portIdentity, portState, logMinDelayReqInterval, peerMeanPathDelay, ….
    pub fn port_state_and_delay(data: &[u8]) -> Option<(u8, f64)> {
        Some((*data.get(10)?, time_interval(data.get(12..20)?)?))
    }

    /// Send `GET id` to `target` and wait for its answer: the response data.
    pub fn ask(
        sock: &std::os::unix::net::UnixDatagram,
        target: &Path,
        id: u16,
        sequence: u16,
    ) -> Option<Vec<u8>> {
        sock.send_to(&get(id, sequence), target).ok()?;
        let mut buf = [0u8; 1500];
        // A clock answers once per port; anything else in the socket is skipped.
        for _ in 0..8 {
            let n = sock.recv(&mut buf).ok()?;
            if let Some(data) = response_data(&buf[..n], id, sequence) {
                return Some(data.to_vec());
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A response as `ptp4l` 4.2 sends it: the request's header, `actionField` RESPONSE,
        /// and the management TLV with its data.
        fn response(id: u16, sequence: u16, data: &[u8]) -> Vec<u8> {
            let mut m = get(id, sequence);
            m.truncate(HEADER + BODY);
            m[HEADER + 12] = RESPONSE;
            m.extend_from_slice(&TLV_MANAGEMENT.to_be_bytes());
            m.extend_from_slice(&(2 + data.len() as u16).to_be_bytes());
            m.extend_from_slice(&id.to_be_bytes());
            m.extend_from_slice(data);
            let len = m.len() as u16;
            m[2..4].copy_from_slice(&len.to_be_bytes());
            m
        }

        #[test]
        fn a_get_is_what_pmc_sends_with_the_profiles_transport() {
            let m = get(CURRENT_DATA_SET, 7);
            assert_eq!(m.len(), 54);
            assert_eq!(m[0], 0x1D, "transportSpecific 1, messageType management");
            assert_eq!(m[1], 2);
            assert_eq!(u16::from_be_bytes([m[2], m[3]]), 54);
            assert_eq!(u16::from_be_bytes([m[30], m[31]]), 7);
            assert_eq!(m[32], 0x04);
            assert!(m[34..44].iter().all(|b| *b == 0xff), "wildcard target");
            assert_eq!(m[46], GET);
            assert_eq!(&m[48..54], &[0x00, 0x01, 0x00, 0x02, 0x20, 0x01]);
        }

        #[test]
        fn a_current_data_set_response_gives_the_offset() {
            // stepsRemoved 1, offsetFromMaster 476 ns, meanPathDelay 0 (as P2P leaves it).
            let mut data = vec![0x00, 0x01];
            data.extend_from_slice(&(476i64 << 16).to_be_bytes());
            data.extend_from_slice(&0i64.to_be_bytes());
            let msg = response(CURRENT_DATA_SET, 4, &data);
            let got = response_data(&msg, CURRENT_DATA_SET, 4).unwrap();
            assert!((current_offset(got).unwrap() - 476e-9).abs() < 1e-15);
            // A negative offset keeps its sign.
            let mut data = vec![0x00, 0x01];
            data.extend_from_slice(&(-79i64 << 16).to_be_bytes());
            data.extend_from_slice(&0i64.to_be_bytes());
            let msg = response(CURRENT_DATA_SET, 4, &data);
            let got = response_data(&msg, CURRENT_DATA_SET, 4).unwrap();
            assert!((current_offset(got).unwrap() + 79e-9).abs() < 1e-15);
        }

        #[test]
        fn a_port_data_set_response_gives_state_and_peer_delay() {
            let mut data = vec![0x66, 0x1b, 0x40, 0xff, 0xfe, 0xf0, 0x1b, 0x96, 0x00, 0x01];
            data.push(UNCALIBRATED);
            data.push(0);
            data.extend_from_slice(&(1497i64 << 16).to_be_bytes());
            data.extend_from_slice(&[0, 3, 0xfd, 2, 0, 2]);
            let msg = response(PORT_DATA_SET, 9, &data);
            let (state, delay) =
                port_state_and_delay(response_data(&msg, PORT_DATA_SET, 9).unwrap()).unwrap();
            assert_eq!(state, UNCALIBRATED);
            assert!((delay - 1497e-9).abs() < 1e-15);
        }

        #[test]
        fn anything_but_the_answer_asked_for_is_skipped() {
            let data = [0u8; 18];
            let msg = response(CURRENT_DATA_SET, 4, &data);
            assert!(
                response_data(&msg, CURRENT_DATA_SET, 5).is_none(),
                "sequence"
            );
            assert!(
                response_data(&msg, PORT_DATA_SET, 4).is_none(),
                "managementId"
            );
            assert!(response_data(&get(CURRENT_DATA_SET, 4), CURRENT_DATA_SET, 4).is_none());
            assert!(
                response_data(&msg[..40], CURRENT_DATA_SET, 4).is_none(),
                "short"
            );
            // A MANAGEMENT_ERROR_STATUS TLV is not a response's data.
            let mut err = msg.clone();
            err[48..50].copy_from_slice(&0x0002u16.to_be_bytes());
            assert!(response_data(&err, CURRENT_DATA_SET, 4).is_none());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNTIME: &str = "/run/user/1000/vxedged";

    /// Every path a rendered command carries is under the run's runtime directory, and none is
    /// linuxptp's default: several runs on one host must not share a socket.
    fn assert_run_scoped(argv: &[String]) {
        for arg in argv {
            assert!(!arg.contains("/var/run"), "default path in {argv:?}");
            assert!(!arg.starts_with("/run/ptp4l"), "default path in {argv:?}");
            assert!(!arg.contains("/tmp"), "fixed path in {argv:?}");
            if arg.starts_with('/') && arg != ENTRYPOINT && !arg.starts_with("/run/netns/") {
                assert!(
                    arg.starts_with(RUNTIME),
                    "`{arg}` is outside the run in {argv:?}"
                );
            }
            if let Some(ns) = arg.strip_prefix("--net=") {
                assert!(ns.starts_with("/run/netns/"), "{arg}");
            }
        }
    }

    #[test]
    fn the_grandmaster_runs_from_the_hold_container_in_the_bridges_namespace() {
        let sock = socket(&grandmaster_dir(Path::new(RUNTIME), "vxbr0"));
        let ports = vec!["vza-ecu1".to_string(), "vza-hpc1".to_string()];
        let argv = grandmaster_command(true, None, 4242, &sock, &ports);
        assert_eq!(
            argv,
            [
                "sudo",
                "-n",
                "nsenter",
                "--target",
                "4242",
                "--mount",
                "--pid",
                "--",
                "/usr/local/bin/gptp",
                "gateway",
                "--uds",
                "/run/user/1000/vxedged/gptp/gm-vxbr0/ptp4l",
                "vza-ecu1",
                "vza-hpc1",
            ]
        );
        assert_run_scoped(&argv);
        // No `--net`: ptp4l stays in the caller's namespace, where the bridge is. A gateway in a
        // named namespace is entered by path. Already privileged, no sudo.
        let argv = grandmaster_command(false, Some("vxtest1"), 4242, &sock, &ports);
        assert_eq!(
            &argv[..5],
            ["nsenter", "--target", "4242", "--mount", "--pid"]
        );
        assert_eq!(argv[5], "--net=/run/netns/vxtest1");
        assert_run_scoped(&argv);
    }

    #[test]
    fn a_follower_runs_free_on_the_nodes_port_with_its_socket_in_the_run() {
        let spec = follower_spec(
            LINUXPTP_IMAGE,
            Path::new(RUNTIME),
            crate::instance::Instance::FIRST,
            "ecu1",
        );
        assert_eq!(spec.name, FOLLOWER_MEMBER);
        assert_eq!(spec.image, LINUXPTP_IMAGE);
        assert_eq!(
            spec.cmd,
            [
                "follower",
                "--uds",
                "/run/user/1000/vxedged/gptp/vx0-ecu1/ptp4l",
                "eth0"
            ]
        );
        assert_eq!(spec.cap_add, ["NET_RAW"]);
        // Mounted at the same path, so a reader's socket path means the same file inside.
        assert_eq!(
            spec.binds,
            [Bind {
                host: PathBuf::from("/run/user/1000/vxedged/gptp/vx0-ecu1"),
                container: "/run/user/1000/vxedged/gptp/vx0-ecu1".into(),
                read_only: false,
                relabel: false,
            }]
        );
        assert_run_scoped(&spec.cmd);
    }

    #[test]
    fn the_grandmaster_group_holds_the_image_with_its_directory_mounted() {
        let group = grandmaster_group(LINUXPTP_IMAGE, "vxbr0", Path::new(RUNTIME)).unwrap();
        assert_eq!(group.name(), "vxgptp-vxbr0");
        assert_eq!(group.infra_network(), InfraNetwork::None);
        assert_eq!(group.infra().image, LINUXPTP_IMAGE);
        assert_eq!(group.infra().cmd, ["hold"]);
        assert_eq!(
            group.infra().binds[0].host,
            PathBuf::from("/run/user/1000/vxedged/gptp/gm-vxbr0")
        );
        // Two gateways, two groups and two directories.
        assert_ne!(
            grandmaster_group_name("vxbr0"),
            grandmaster_group_name("vxbr1")
        );
        assert_ne!(
            grandmaster_dir(Path::new(RUNTIME), "vxbr0"),
            grandmaster_dir(Path::new(RUNTIME), "vxbr1")
        );
    }

    #[test]
    fn a_socket_path_too_long_for_a_unix_address_is_refused() {
        let long = PathBuf::from(format!("/{}", "r".repeat(100)));
        assert!(matches!(
            prepare_dir(&follower_dir(
                &long,
                crate::instance::Instance::FIRST,
                "ecu1"
            )),
            Err(GptpError::SocketPathTooLong { .. })
        ));
    }

    #[test]
    fn readings_render_as_one_gauge_per_family_by_entity() {
        assert_eq!(render(&[]), "");
        let text = render(&[(
            "veh/zones/front/ecu1".to_string(),
            Reading {
                offset_s: -4.76e-7,
                path_delay_s: 1.497e-6,
            },
        )]);
        assert!(
            text.contains(
                "vxedge_gptp_offset_seconds{entity=\"veh/zones/front/ecu1\"} -0.000000476"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "vxedge_gptp_path_delay_seconds{entity=\"veh/zones/front/ecu1\"} 0.000001497"
            ),
            "{text}"
        );
        assert!(text.contains("# TYPE vxedge_gptp_offset_seconds gauge"));
    }

    #[test]
    fn a_follower_that_does_not_answer_reads_as_none() {
        let dir = std::env::temp_dir().join(format!("vxgptp-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_follower(&dir), None);
        // Its query socket is not left behind.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_workspace_with_time_sync_runs_the_image() {
        let plain = "kind: experiment\nid: e\nvehicles: [veh]\nnetwork:\n  slices:\n    - { id: adas, mode: vlan, vlan: 101 }\n---\nkind: vehicle\nid: veh\ndevices: {}\n";
        let m = crate::model::load_yaml(plain).unwrap();
        assert!(gptp_images(&m).is_empty());
        assert_eq!(
            metrics(&m, Path::new(RUNTIME), crate::instance::Instance::FIRST),
            ""
        );
    }
}
