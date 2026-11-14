//! Running declared measurements (decision X-20): the daemon runs the host's `vxprobe` inside
//! each end's network namespace, so a probe measures the path the experiment declares and never
//! a host-side shortcut.
//!
//! - The sender runs in the `from` node's namespace and sends to the reflector.
//! - The reflector runs in the `to` node's namespace, or for `to: internet` on the far side of
//!   the shared uplink: a VLAN subinterface of the uplink's peer for the slice, addressed in the
//!   sender's subnet. That is the far side exp-001 builds by hand.
//! - A `to` node that boots a guest runs its own reflector, on the port its part declares
//!   ([`declared_reflector`]): nothing is started in it, and the probe is aimed at that port.
//! - `from: host` sends from the host's namespace through the far side of the `to` node's subnet,
//!   so a probe reaches a guest over the slice the way exp-002's runner reached each arm.
//! - Before the sender starts, the reflector must answer a datagram from where the sender will
//!   run, within [`REFLECTOR_WAIT`]: a guest still booting is waited for, and one that never
//!   answers fails the measurement rather than recording a window of pure loss.
//! - On a `mode: s-nssai` slice, the reflector runs in the slice's UE, bound to its PDU-session
//!   address, and the sender runs on the host, which reaches the UE through the 5G core's uplink:
//!   the path the core routes, as `TrafficService` probes a UE. The figures are round trips, so
//!   they read the same either way round.
//! - The sender serves its metrics on `127.0.0.1` inside its own namespace. The daemon's exporter
//!   enters the namespace to read them and serves them as `measurements/<id>`
//!   ([`collect_metrics`]), so a scrape never crosses the slice it measures.
//!
//! [`plan`] is pure: where each end runs, from the model. The rest touches the host, and keeps
//! its state in the runtime folder (`measure-<id>.*`) as the traffic and service lifecycles do,
//! so a restarted daemon can still stop what an earlier one started.

use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::INTERNET;
use crate::containers::LazyClient;
use crate::gateway::netlink;
use crate::gateway::{GatewayPlan, Shell};
use crate::model::Experiment;
use crate::zones::NodePlan;

/// The first port a reflector listens on; measurement `i` (in id order) uses this plus `i`. Far
/// from the ports node payloads use, since a reflector shares its node's namespace with them.
pub const REFLECT_PORT_BASE: u16 = 19800;
/// The first port a sender serves its metrics on, inside its own namespace.
pub const METRICS_PORT_BASE: u16 = 19900;
/// How long a measurement waits for its reflector to answer before it fails: long enough for a
/// guest to boot under emulation.
pub const REFLECTOR_WAIT: Duration = Duration::from_secs(90);
/// The bundle file a guest's part declares its reflector in.
pub const BUNDLE_META: &str = "meta.json";
/// The key that declares it: `udp/<port>`.
pub const REFLECTOR_KEY: &str = "org.vxedge.reflector";

/// One end that runs in a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct End {
    pub node: String,
    /// The node's namespace name, or its container group's name when it runs an image.
    pub runtime_name: String,
    /// Whether the node runs an OCI image, so its namespace is its infra container's.
    pub image: bool,
    /// Its address on the measurement's slice, without the prefix.
    pub addr: String,
}

/// A VLAN subinterface of the uplink's peer, in the host's namespace, addressed in a node's
/// subnet: the far side of the shared uplink for that node's slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FarSide {
    pub parent: String,
    pub vlan: u16,
    pub iface: String,
    /// The address with its prefix, as `ip addr` takes it.
    pub cidr: String,
    pub addr: String,
}

/// Where the sender runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sender {
    Node(End),
    /// The host, on the data-network side of the 5G core, for a measurement from a UE.
    DataNetwork {
        /// The device whose UE it measures.
        node: String,
    },
    /// The host, through the far side of the `to` node's subnet (`from: host`).
    Host(FarSide),
}

impl Sender {
    /// The device the measurement is from, or `host`.
    pub fn node(&self) -> &str {
        match self {
            Self::Node(end) => &end.node,
            Self::DataNetwork { node } => node,
            Self::Host(_) => super::HOST,
        }
    }
}

/// Where the reflector runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reflector {
    Node(End),
    /// The slice's UE, bound to its PDU-session address, which is learned when it starts.
    Ue(Box<crate::fiveg::SliceSession>),
    /// A VLAN subinterface of the uplink's peer, in the host's namespace.
    FarSide(FarSide),
    /// The reflector a node's guest runs, declared by its part in the bundle's `meta.json`.
    Guest {
        node: String,
        addr: String,
        /// Where the bundle's `meta.json` is, or will be once `up` has fetched it.
        meta: PathBuf,
    },
}

impl Reflector {
    /// Its address, when the plan knows it: a UE's is learned when the measurement starts.
    pub fn addr(&self) -> Option<&str> {
        match self {
            Self::Node(end) => Some(&end.addr),
            Self::FarSide(f) => Some(&f.addr),
            Self::Guest { addr, .. } => Some(addr),
            Self::Ue(_) => None,
        }
    }
}

/// Where one measurement runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub id: String,
    /// The slot of the run measured: a UE reflector lives in that run's 5G pods.
    pub instance: crate::instance::Instance,
    pub entity: String,
    pub sender: Sender,
    pub reflector: Reflector,
    /// The port a reflector vxedge starts listens on. A guest's reflector listens on the port its
    /// part declares instead, read when the measurement starts.
    pub reflect_port: u16,
    pub metrics_port: u16,
    pub rate_pps: u32,
    pub payload_bytes: u32,
}

/// Where each measurement runs, in id order. The model is assumed to pass
/// [`super::problems`]; what it cannot say is an error here. `cache_dir` is the part cache a
/// published guest bundle is fetched into.
pub fn plan(
    model: &Experiment,
    gateway: &GatewayPlan,
    nodes: &[NodePlan],
    cache_dir: &Path,
) -> Result<Vec<Placement>, String> {
    model
        .measurements
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let i = u16::try_from(i).map_err(|_| "too many measurements".to_string())?;
            let what = format!("measurement `{}`", m.id);
            let (sender, reflector) = if super::is_snssai(model, &m.slice) {
                (
                    Sender::DataNetwork {
                        node: m.from.clone(),
                    },
                    Reflector::Ue(Box::new(ue_session(model, &m.slice, &what)?)),
                )
            } else {
                let sender = if m.from == super::HOST {
                    Sender::Host(far_side_of(model, gateway, &m.to, &m.slice, &what)?)
                } else {
                    Sender::Node(end_on(model, nodes, &m.from, &m.slice, &what)?)
                };
                let reflector = if m.to == INTERNET {
                    Reflector::FarSide(far_side_of(model, gateway, &m.from, &m.slice, &what)?)
                } else {
                    reflector_on(model, nodes, &m.to, &m.slice, cache_dir, &what)?
                };
                (sender, reflector)
            };
            Ok(Placement {
                id: m.id.clone(),
                instance: gateway.instance,
                entity: m.entity(),
                sender,
                reflector,
                reflect_port: REFLECT_PORT_BASE + i,
                metrics_port: METRICS_PORT_BASE + i,
                rate_pps: m.rate_pps,
                payload_bytes: m.payload_bytes,
            })
        })
        .collect()
}

/// The 5G session of the `s-nssai` slice `slice`.
fn ue_session(
    model: &Experiment,
    slice: &str,
    what: &str,
) -> Result<crate::fiveg::SliceSession, String> {
    let plan = crate::fiveg::FiveGPlan::from_model(model).map_err(|e| format!("{what}: {e}"))?;
    plan.sessions
        .into_iter()
        .find(|s| s.slice_id == slice)
        .ok_or_else(|| format!("{what}: slice `{slice}` has no 5G session"))
}

/// `node` as the reflecting end on `slice`: the guest's own reflector when the node boots one,
/// else a reflector vxedge starts in its namespace.
fn reflector_on(
    model: &Experiment,
    nodes: &[NodePlan],
    node: &str,
    slice: &str,
    cache_dir: &Path,
    what: &str,
) -> Result<Reflector, String> {
    let end = end_on(model, nodes, node, slice, what)?;
    let np = nodes
        .iter()
        .find(|n| n.node_id == node)
        .ok_or_else(|| format!("{what}: `{node}` has no node plan"))?;
    if !super::is_guest(np.backend) {
        return Ok(Reflector::Node(end));
    }
    let dir = match (&np.bundle, &np.remote_bundle) {
        (Some(dir), _) => dir.clone(),
        (None, Some((_, digest))) => {
            crate::registry::cache_path(cache_dir, digest).map_err(|e| format!("{what}: {e}"))?
        }
        (None, None) => {
            return Err(format!(
                "{what}: `{node}` boots no bundle, so nothing declares a reflector in it"
            ));
        }
    };
    Ok(Reflector::Guest {
        node: node.to_string(),
        addr: end.addr,
        meta: dir.join(BUNDLE_META),
    })
}

/// What placing the measurements says is wrong, as measurement problems: a plan that cannot be
/// made, and each guest `to` whose bundle is on disk and declares no reflector. A published
/// bundle not yet fetched is read when its measurement starts, after `up` has fetched it.
pub fn placement_problems(
    model: &Experiment,
    nodes: &[NodePlan],
    cache_dir: &Path,
) -> Vec<crate::intent::Problem> {
    let problem = |field: String, detail: String| crate::intent::Problem {
        kind: crate::intent::ProblemKind::Measurement,
        experiment: model.id.clone(),
        field,
        detail,
    };
    if model.measurements.is_empty() {
        return Vec::new();
    }
    // The problems do not depend on the slot; the nodes' own keeps any name in an error theirs.
    let instance = nodes
        .first()
        .map_or(crate::instance::Instance::FIRST, |n| n.instance);
    let placements = GatewayPlan::from_model(model, instance)
        .map_err(|e| e.to_string())
        .and_then(|gateway| plan(model, &gateway, nodes, cache_dir));
    match placements {
        Err(e) => vec![problem("measurements".into(), e)],
        Ok(placements) => placements
            .iter()
            .filter_map(|p| match &p.reflector {
                Reflector::Guest { node, meta, .. } if meta.exists() => {
                    declared_reflector(meta).err().map(|e| {
                        problem(
                            format!("measurements.{}.to", p.id),
                            format!("`{node}`: {e}"),
                        )
                    })
                }
                _ => None,
            })
            .collect(),
    }
}

/// The UDP port the reflector a guest bundle's `meta.json` declares listens on
/// (`org.vxedge.reflector: udp/<port>`), or why there is none to aim at.
pub fn declared_reflector(meta: &Path) -> Result<u16, String> {
    let text = std::fs::read_to_string(meta)
        .map_err(|e| format!("cannot read `{}`: {e}", meta.display()))?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("`{}` is not JSON: {e}", meta.display()))?;
    let Some(declared) = json.get(REFLECTOR_KEY) else {
        return Err(format!(
            "its part declares no `{REFLECTOR_KEY}`, so nothing says its guest answers probes: \
             a guest is a measurement's `to` only when its part declares the reflector it runs, \
             e.g. `{REFLECTOR_KEY}: udp/9800`"
        ));
    };
    declared
        .as_str()
        .and_then(|d| d.strip_prefix("udp/"))
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p != 0)
        .ok_or_else(|| {
            format!("its part declares `{REFLECTOR_KEY}: {declared}`, which is not `udp/<port>`")
        })
}

/// `node` as an end on `slice`: its namespace and its pinned address there. `what` names the
/// measurement or profile in errors.
pub(crate) fn end_on(
    model: &Experiment,
    nodes: &[NodePlan],
    node: &str,
    slice: &str,
    what: &str,
) -> Result<End, String> {
    let endpoint = model
        .entities
        .iter()
        .find_map(|v| super::endpoint_on(v, node, slice))
        .ok_or_else(|| format!("{what}: `{node}` has no link on slice `{slice}`"))?;
    let cidr = endpoint
        .ip_cidr
        .as_deref()
        .ok_or_else(|| format!("{what}: `{node}` pins no `ip` on slice `{slice}`"))?;
    let np = nodes
        .iter()
        .find(|n| n.node_id == node)
        .ok_or_else(|| format!("{what}: `{node}` has no node plan"))?;
    Ok(End {
        node: node.to_string(),
        runtime_name: np.runtime_name(),
        image: np.image.is_some(),
        addr: cidr.split('/').next().unwrap_or(cidr).to_string(),
    })
}

/// The far side for the node `from` on `slice`: the first host address of its subnet, on the
/// slice's VLAN.
pub(crate) fn far_side_of(
    model: &Experiment,
    gateway: &GatewayPlan,
    from: &str,
    slice_id: &str,
    what: &str,
) -> Result<FarSide, String> {
    let slice = gateway
        .slices
        .iter()
        .find(|s| s.id == slice_id)
        .ok_or_else(|| format!("{what}: slice `{slice_id}` is not a VLAN slice of the gateway"))?;
    let cidr = model
        .entities
        .iter()
        .find_map(|v| super::endpoint_on(v, from, slice_id))
        .and_then(|e| e.ip_cidr.clone())
        .unwrap_or_default();
    let addr =
        first_host(&cidr).ok_or_else(|| format!("{what}: `{cidr}` is not an IPv4 subnet"))?;
    let taken = model
        .entities
        .iter()
        .flat_map(|v| &v.links)
        .flat_map(|l| &l.endpoints)
        .filter_map(|e| e.ip_cidr.as_deref())
        .find(|c| c.split('/').next() == Some(addr.as_str()));
    if let Some(taken) = taken {
        return Err(format!(
            "{what}: the far side takes {addr}, the first address of `{from}`'s subnet, and a \
             link already pins `{taken}`"
        ));
    }
    let prefix = cidr.split_once('/').map(|(_, p)| p).unwrap_or("24");
    Ok(FarSide {
        parent: gateway.uplink_peer.clone(),
        vlan: slice.vlan,
        iface: format!("{}.{}", gateway.uplink_peer, slice.vlan),
        cidr: format!("{addr}/{prefix}"),
        addr,
    })
}

/// The first host address of an IPv4 subnet given as `a.b.c.d/p`.
pub(crate) fn first_host(cidr: &str) -> Option<String> {
    let (ip, prefix) = cidr.split_once('/')?;
    let ip: Ipv4Addr = ip.parse().ok()?;
    let prefix: u32 = prefix.parse().ok()?;
    if !(1..=30).contains(&prefix) {
        return None;
    }
    let mask = u32::MAX << (32 - prefix);
    Some(Ipv4Addr::from((u32::from(ip) & mask) + 1).to_string())
}

// ---------------------------------------------------------------------------------------------
// Host side
// ---------------------------------------------------------------------------------------------

/// What a started measurement left in the runtime folder, for `stop` and the exporter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Started {
    pub id: String,
    pub entity: String,
    /// The sender's namespace: a name under `/run/netns`, a `/proc/<pid>/ns/net` path, or empty
    /// for the host's own.
    pub netns: String,
    pub metrics_port: u16,
    /// The far-side interface this measurement's reflector listens on, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub far_side: Option<String>,
}

/// The file prefix of a measurement's processes, records and logs.
const MEASURE: &str = "measure";
/// The file prefix `traffic::lifecycle` uses, so a profile run inside a node is read and stopped
/// by the same status and `down` as one run on the host.
pub(crate) const TRAFFIC: &str = "traffic";

fn file_of(prefix: &str, runtime_dir: &Path, id: &str, suffix: &str) -> PathBuf {
    runtime_dir.join(format!("{prefix}-{id}{suffix}"))
}

fn file(runtime_dir: &Path, id: &str, suffix: &str) -> PathBuf {
    file_of(MEASURE, runtime_dir, id, suffix)
}

fn pidfile(runtime_dir: &Path, id: &str, role: &str) -> PathBuf {
    file(runtime_dir, id, &format!("-{role}.pid"))
}

fn exitfile(runtime_dir: &Path, id: &str, role: &str) -> PathBuf {
    file(runtime_dir, id, &format!("-{role}.exit"))
}

/// The summary `vxprobe send` prints when it finishes: its stdout.
pub fn summary_file(runtime_dir: &Path, id: &str) -> PathBuf {
    file(runtime_dir, id, ".summary.json")
}

fn read_pid(runtime_dir: &Path, id: &str, role: &str) -> Option<i32> {
    read_pid_of(MEASURE, runtime_dir, id, role)
}

pub(crate) fn read_pid_of(prefix: &str, runtime_dir: &Path, id: &str, role: &str) -> Option<i32> {
    std::fs::read_to_string(file_of(prefix, runtime_dir, id, &format!("-{role}.pid")))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub(crate) fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Where one measurement stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    NotStarted,
    Running,
    Finished,
    Failed(String),
    /// It stopped, and nothing recorded how: the daemon that reaped it was not this one.
    Unknown,
}

/// The state of measurement `id`, decided by its sender: a reflector serves until stopped.
pub fn status(runtime_dir: &Path, id: &str) -> State {
    let Some(pid) = read_pid(runtime_dir, id, "send") else {
        return State::NotStarted;
    };
    if alive(pid) {
        return State::Running;
    }
    match std::fs::read_to_string(exitfile(runtime_dir, id, "send")) {
        Ok(code) if code.trim() == "0" => State::Finished,
        Ok(code) => State::Failed(format!("the sender exited with {}", code.trim())),
        Err(_) => State::Unknown,
    }
}

/// Every measurement started under `runtime_dir` and not stopped.
pub fn started(runtime_dir: &Path) -> Vec<Started> {
    let Ok(dir) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    let mut out: Vec<Started> = dir
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("measure-")
                && name.ends_with(".json")
                && !name.ends_with(".summary.json")
        })
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Start `vxprobe` as `role` of measurement `id`, inside `netns` (the host's when `None`),
/// tracked by a pidfile and reaped on a thread that keeps its exit code. stdout goes to `stdout`
/// when given, else with stderr to the role's log.
fn spawn(
    runtime_dir: &Path,
    id: &str,
    role: &'static str,
    netns: Option<&str>,
    bin: &str,
    args: Vec<String>,
    stdout: Option<PathBuf>,
) -> Result<u32, String> {
    spawn_as(MEASURE, runtime_dir, id, role, netns, bin, args, stdout)
}

/// [`spawn`] with the files named `<prefix>-<id>-<role>.*`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_as(
    prefix: &'static str,
    runtime_dir: &Path,
    id: &str,
    role: &'static str,
    netns: Option<&str>,
    bin: &str,
    args: Vec<String>,
    stdout: Option<PathBuf>,
) -> Result<u32, String> {
    let log_path = file_of(prefix, runtime_dir, id, &format!("-{role}.log"));
    let (rt, id_s, bin_s) = (runtime_dir.to_path_buf(), id.to_string(), bin.to_string());
    let start = move || -> Result<u32, String> {
        let log = std::fs::File::create(&log_path)
            .map_err(|e| format!("cannot write `{}`: {e}", log_path.display()))?;
        let out = match &stdout {
            Some(p) => std::fs::File::create(p)
                .map_err(|e| format!("cannot write `{}`: {e}", p.display()))?,
            None => log
                .try_clone()
                .map_err(|e| format!("cannot write `{}`: {e}", log_path.display()))?,
        };
        let mut child = Command::new(&bin_s)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(log))
            .spawn()
            .map_err(|e| format!("cannot run `{bin_s}`: {e}"))?;
        let pid = child.id();
        let exit = file_of(prefix, &rt, &id_s, &format!("-{role}.exit"));
        let _ = std::fs::remove_file(&exit);
        std::fs::write(
            file_of(prefix, &rt, &id_s, &format!("-{role}.pid")),
            pid.to_string(),
        )
        .map_err(|e| format!("cannot write the pidfile: {e}"))?;
        std::thread::spawn(move || {
            let code = match child.wait() {
                Ok(status) => status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "a signal".to_string()),
                Err(e) => format!("unknown ({e})"),
            };
            let _ = std::fs::write(exit, code);
        });
        Ok(pid)
    };
    match netns {
        None => start(),
        Some(ns) => {
            netlink::in_netns(ns, start).map_err(|e| format!("cannot enter `{ns}`: {e}"))?
        }
    }
}

/// The namespace an end runs in: its infra container's when it runs an image, else its named
/// namespace. Either must exist: the node's zone is up.
pub(crate) async fn netns_of(end: &End, containers: &LazyClient) -> Result<String, String> {
    if end.image {
        let client = containers.get().await.map_err(|e| e.to_string())?;
        let pid = client
            .member_pid(&end.runtime_name, crate::containers::spec::INFRA_MEMBER)
            .await
            .ok_or_else(|| format!("`{}` is not running: bring its zone up first", end.node))?;
        Ok(format!("/proc/{pid}/ns/net"))
    } else if netlink::netns_exists(&end.runtime_name) {
        Ok(end.runtime_name.clone())
    } else {
        Err(format!(
            "`{}` has no network namespace: bring its zone up first",
            end.node
        ))
    }
}

/// The far side's VLAN subinterface of the uplink's peer, up and addressed. Idempotent.
pub(crate) fn ensure_far_side(
    sh: &Shell,
    parent: &str,
    vlan: u16,
    iface: &str,
    cidr: &str,
) -> Result<(), String> {
    if !sh.link_exists(iface) {
        let vlan = vlan.to_string();
        sh.run(
            "ip",
            &[
                "link", "add", "link", parent, "name", iface, "type", "vlan", "id", &vlan,
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    sh.run("ip", &["addr", "replace", cidr, "dev", iface])
        .map_err(|e| e.to_string())?;
    sh.run("ip", &["link", "set", iface, "up"])
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Whether a running traffic profile sends to the far side `iface` (`crate::traffic::in_node`).
fn traffic_uses(runtime_dir: &Path, iface: &str) -> bool {
    let Ok(dir) = std::fs::read_dir(runtime_dir) else {
        return false;
    };
    dir.flatten().any(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(id) = name
            .strip_prefix("traffic-")
            .and_then(|n| n.strip_suffix(".in-node.json"))
        else {
            return false;
        };
        let uses = std::fs::read(e.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| v["far_side"].as_str() == Some(iface));
        uses && ["send", "load", "reflect"]
            .iter()
            .any(|role| read_pid_of(TRAFFIC, runtime_dir, id, role).is_some_and(alive))
    })
}

/// Start one measurement for `duration_s`: its reflector, then its sender once the reflector
/// answers. A measurement whose sender still runs is left as it is.
pub async fn start(
    sh: &Shell,
    containers: &LazyClient,
    runtime_dir: &Path,
    vxprobe_bin: &str,
    p: &Placement,
    duration_s: u64,
) -> Result<String, String> {
    if status(runtime_dir, &p.id) == State::Running {
        return Ok(format!("measurement `{}` already running", p.id));
    }
    let err = |e: String| format!("measurement `{}`: {e}", p.id);
    // The host's own namespace is the empty name: a spawn there enters none.
    let (sender_ns, sender_far_side) = match &p.sender {
        Sender::Node(end) => (netns_of(end, containers).await.map_err(err)?, None),
        Sender::DataNetwork { .. } => (String::new(), None),
        Sender::Host(f) => {
            let (sh, f) = (sh.clone(), f.clone());
            let iface = f.iface.clone();
            tokio::task::spawn_blocking(move || {
                ensure_far_side(&sh, &f.parent, f.vlan, &f.iface, &f.cidr)
            })
            .await
            .map_err(|e| err(e.to_string()))?
            .map_err(err)?;
            (String::new(), Some(iface))
        }
    };
    let listen = |addr: &str| {
        vec![
            "reflect".into(),
            "--listen".into(),
            format!("{addr}:{}", p.reflect_port),
        ]
    };
    // A reflector outlives its sender: a measurement started again after its sender finished
    // keeps the one already listening, which still holds the port.
    let reflecting = read_pid(runtime_dir, &p.id, "reflect").is_some_and(alive);
    let mut target = p.reflector.addr().map(str::to_string);
    let mut port = p.reflect_port;
    let reflector_far_side = match &p.reflector {
        Reflector::Ue(session) => {
            // The UE's reflector is a member of its pod, named after the measurement, and stays
            // until the 5G core comes down; starting it again finds it and learns the address.
            let client = containers.get().await.map_err(|e| err(e.to_string()))?;
            let cfg = crate::fiveg::FiveGLifecycleConfig::for_instance(p.instance);
            let (addr, _) = crate::fiveg::ue_reflector_up(
                client,
                &cfg,
                &runtime_dir.join("fiveg-config"),
                session,
                p.reflect_port,
                &format!("measure-{}", p.id),
            )
            .await
            .map_err(|e| err(format!("the UE of slice `{}`: {e}", session.slice_id)))?;
            target = Some(addr);
            None
        }
        // The guest runs its own reflector: nothing to start, only the port to aim at.
        Reflector::Guest { node, meta, .. } => {
            port = declared_reflector(meta).map_err(|e| err(format!("`{node}`: {e}")))?;
            None
        }
        Reflector::Node(_) if reflecting => None,
        Reflector::FarSide(f) if reflecting => Some(f.iface.clone()),
        Reflector::Node(to) => {
            let ns = netns_of(to, containers).await.map_err(err)?;
            let (rt, id, bin, args) = (
                runtime_dir.to_path_buf(),
                p.id.clone(),
                vxprobe_bin.to_string(),
                listen("0.0.0.0"),
            );
            tokio::task::spawn_blocking(move || {
                spawn(&rt, &id, "reflect", Some(&ns), &bin, args, None)
            })
            .await
            .map_err(|e| err(e.to_string()))?
            .map_err(err)?;
            None
        }
        Reflector::FarSide(f) => {
            let (sh, f_c) = (sh.clone(), f.clone());
            let (rt, id, bin, args) = (
                runtime_dir.to_path_buf(),
                p.id.clone(),
                vxprobe_bin.to_string(),
                listen(&f.addr),
            );
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                ensure_far_side(&sh, &f_c.parent, f_c.vlan, &f_c.iface, &f_c.cidr)?;
                spawn(&rt, &id, "reflect", None, &bin, args, None).map(|_| ())
            })
            .await
            .map_err(|e| err(e.to_string()))?
            .map_err(err)?;
            Some(f.iface.clone())
        }
    };
    let target = target.ok_or_else(|| err("no reflector address".into()))?;
    // The first probe goes out only once the reflector answers from where the sender runs: a
    // guest may still be booting, and a vxprobe reflector takes a moment to bind.
    let (ns, addr) = (sender_ns.clone(), target.clone());
    let answered = tokio::task::spawn_blocking(move || wait_for_reflector(&ns, &addr, port))
        .await
        .map_err(|e| err(e.to_string()))?;
    if !answered {
        let hint = match &p.reflector {
            Reflector::Guest { node, .. } => format!(
                ": `{node}`'s guest may not have booted, or runs no reflector on that port. Its \
                 console log is in the daemon's runtime folder"
            ),
            _ => String::new(),
        };
        return Err(err(format!(
            "the reflector at {target}:{port} did not answer within {} s{hint}",
            REFLECTOR_WAIT.as_secs()
        )));
    }
    let args = vec![
        "send".to_string(),
        "--target".into(),
        format!("{target}:{port}"),
        "--pps".into(),
        p.rate_pps.to_string(),
        "--payload-bytes".into(),
        p.payload_bytes.to_string(),
        "--duration-s".into(),
        duration_s.to_string(),
        "--metrics".into(),
        format!("127.0.0.1:{}", p.metrics_port),
    ];
    let (rt, id, bin, ns) = (
        runtime_dir.to_path_buf(),
        p.id.clone(),
        vxprobe_bin.to_string(),
        sender_ns.clone(),
    );
    let summary = summary_file(runtime_dir, &p.id);
    tokio::task::spawn_blocking(move || {
        let ns = (!ns.is_empty()).then_some(ns.as_str());
        spawn(&rt, &id, "send", ns, &bin, args, Some(summary))
    })
    .await
    .map_err(|e| err(e.to_string()))?
    .map_err(err)?;
    let record = Started {
        id: p.id.clone(),
        entity: p.entity.clone(),
        netns: sender_ns,
        metrics_port: p.metrics_port,
        far_side: reflector_far_side.or(sender_far_side),
    };
    std::fs::write(
        file(runtime_dir, &p.id, ".json"),
        serde_json::to_vec_pretty(&record).unwrap_or_default(),
    )
    .map_err(|e| err(format!("cannot write its record: {e}")))?;
    Ok(format!(
        "measurement `{}` started: {} pps from `{}` to {target}:{port} for {duration_s} s",
        p.id,
        p.rate_pps,
        p.sender.node()
    ))
}

/// Whether the reflector at `addr:port` echoes a datagram sent from the namespace `netns` (the
/// host's when empty) within [`REFLECTOR_WAIT`]. Every reflector vxedge aims at echoes what it
/// receives.
fn wait_for_reflector(netns: &str, addr: &str, port: u16) -> bool {
    let Ok(ip) = addr.parse::<Ipv4Addr>() else {
        return false;
    };
    let target = std::net::SocketAddr::from((ip, port));
    let poll = move || {
        let deadline = std::time::Instant::now() + REFLECTOR_WAIT;
        let Ok(sock) = std::net::UdpSocket::bind(("0.0.0.0", 0)) else {
            return false;
        };
        if sock
            .set_read_timeout(Some(Duration::from_millis(500)))
            .is_err()
        {
            return false;
        }
        let payload = b"vxedge-reflector-wait";
        let mut buf = [0u8; 64];
        while std::time::Instant::now() < deadline {
            // A send can fail while the guest's address is not yet resolvable; try again.
            if sock.send_to(payload, target).is_ok() {
                while let Ok((n, from)) = sock.recv_from(&mut buf) {
                    if from == target && &buf[..n] == payload {
                        return true;
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        false
    };
    if netns.is_empty() {
        poll()
    } else {
        netlink::in_netns(netns, poll).unwrap_or(false)
    }
}

/// Stop measurement `id`: its sender and reflector, and its far-side interface when no other
/// started measurement uses it. Idempotent.
pub fn stop(sh: &Shell, runtime_dir: &Path, id: &str) -> String {
    for role in ["send", "reflect"] {
        if let Some(pid) = read_pid(runtime_dir, id, role) {
            let _ = Command::new("kill").arg(pid.to_string()).output();
            for _ in 0..10 {
                if !alive(pid) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if alive(pid) {
                let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
            }
        }
        let _ = std::fs::remove_file(pidfile(runtime_dir, id, role));
        let _ = std::fs::remove_file(exitfile(runtime_dir, id, role));
    }
    let record = file(runtime_dir, id, ".json");
    let far_side = std::fs::read(&record)
        .ok()
        .and_then(|b| serde_json::from_slice::<Started>(&b).ok())
        .and_then(|s| s.far_side);
    let _ = std::fs::remove_file(&record);
    if let Some(iface) = far_side {
        let shared = started(runtime_dir)
            .iter()
            .any(|s| s.far_side.as_deref() == Some(iface.as_str()))
            || traffic_uses(runtime_dir, &iface);
        if !shared && sh.link_exists(&iface) {
            let _ = sh.run("ip", &["link", "del", &iface]);
        }
    }
    format!("measurement `{id}` stopped")
}

/// Every running measurement's metrics, read inside its sender's namespace and labelled with its
/// entity, as Prometheus text. One that does not answer within half a second is left out, so a
/// hung sender cannot hold a scrape past the ingest's timeout.
pub fn collect_metrics(runtime_dir: &Path) -> String {
    let mut out = String::new();
    for s in started(runtime_dir) {
        if status(runtime_dir, &s.id) != State::Running {
            continue;
        }
        let port = s.metrics_port;
        let text = if s.netns.is_empty() {
            fetch(port)
        } else {
            netlink::in_netns(&s.netns, move || fetch(port))
                .ok()
                .flatten()
        };
        let Some(text) = text else {
            continue;
        };
        out.push_str(&relabel(&text, &s.entity));
    }
    out
}

/// `GET /metrics` from `127.0.0.1:port`, in the calling thread's namespace.
fn fetch(port: u16) -> Option<String> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n\r\n")
        .ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).ok()?;
    let (head, body) = raw.split_once("\r\n\r\n")?;
    head.starts_with("HTTP/1.1 200").then(|| body.to_string())
}

/// Prometheus text with `entity="<entity>"` added to every sample. Comment lines are dropped:
/// the samples are what the ingest reads, and two measurements would repeat one family's.
pub fn relabel(text: &str, entity: &str) -> String {
    let label = format!("entity=\"{entity}\"");
    let mut out = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let relabelled = match (line.find('{'), line.find(' ')) {
            (Some(b), Some(s)) if b < s => {
                let rest = &line[b + 1..];
                let sep = if rest.starts_with('}') { "" } else { "," };
                format!("{}{{{label}{sep}{rest}", &line[..b])
            }
            (_, Some(s)) => format!("{}{{{label}}}{}", &line[..s], &line[s..]),
            _ => continue,
        };
        out.push_str(&relabelled);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const WS: &str = r#"
kind: vehicle
id: car
zones: { front: {}, rear: {} }
devices:
  ecu1: { role: zone-controller, zone: front, backend: container }
  cap1: { role: sensor, zone: front, backend: wasm-capsule }
  ecu2: { role: zone-controller, zone: rear, backend: container }
functions:
  f1: { deployed_on: ecu1 }
  f2: { deployed_on: ecu2 }
  f3: { deployed_on: cap1 }
links:
  - { id: l1, endpoints: [{ node: ecu1, port: eth0, ip: 10.101.0.2/24 }], slice: adas }
  - { id: l2, endpoints: [{ node: cap1, port: eth0, ip: 10.101.0.3/24 }], slice: adas }
  - { id: l3, endpoints: [{ node: ecu2, port: eth0, ip: 10.103.0.1/24 }], slice: info }
---
kind: baseline
id: bl
parts:
  - { ref: 'local/vxprobe:host-process', target_ids: f1 }
  - { ref: ./capsule/reflector.wasm, kind: wasm, target_ids: f3 }
  - { ref: ghcr.io/vxedge/parts/zone/linux/app/probe, kind: container, digest: 'sha256:2222222222222222222222222222222222222222222222222222222222222222', target_ids: f2 }
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

    fn placements(measurements: &str) -> Result<Vec<Placement>, String> {
        let model = crate::model::load_yaml(&format!("{WS}measurements:\n{measurements}")).unwrap();
        let gateway = GatewayPlan::from_model(&model, crate::instance::Instance::FIRST).unwrap();
        let nodes =
            crate::zones::plan_nodes_from_model(&model, &gateway, Path::new("/nonexistent"))
                .unwrap();
        plan(&model, &gateway, &nodes, Path::new("/nonexistent"))
    }

    #[test]
    fn a_probe_to_the_uplink_reflects_on_the_first_address_of_the_senders_subnet() {
        let p = placements(
            "  ctl-rtt: { kind: rtt-probe, from: ecu1, to: internet, slice: adas, rate_pps: 500 }\n",
        )
        .unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].entity, "measurements/ctl-rtt");
        let Sender::Node(sender) = &p[0].sender else {
            panic!("{:?}", p[0].sender)
        };
        assert_eq!(sender.addr, "10.101.0.2");
        assert!(!sender.image);
        let Reflector::FarSide(FarSide {
            parent,
            vlan,
            iface,
            cidr,
            addr,
        }) = &p[0].reflector
        else {
            panic!("{:?}", p[0].reflector)
        };
        assert_eq!(*vlan, 101);
        assert_eq!(iface, &format!("{parent}.101"));
        assert_eq!(
            (cidr.as_str(), addr.as_str()),
            ("10.101.0.1/24", "10.101.0.1")
        );
        assert_eq!((p[0].reflect_port, p[0].metrics_port), (19800, 19900));
    }

    #[test]
    fn a_probe_between_nodes_reflects_in_the_target_node() {
        let p = placements(
            "  a: { kind: rtt-probe, from: cap1, to: ecu1, slice: adas }\n  \
             b: { kind: rtt-probe, from: ecu1, to: cap1, slice: adas }\n",
        )
        .unwrap();
        assert_eq!(p[1].reflect_port, 19801);
        let Reflector::Node(to) = &p[0].reflector else {
            panic!()
        };
        assert_eq!((to.node.as_str(), to.addr.as_str()), ("ecu1", "10.101.0.2"));
    }

    #[test]
    fn a_far_side_address_a_link_already_pins_is_refused() {
        let err = placements("  a: { kind: rtt-probe, from: ecu2, to: internet, slice: info }\n")
            .unwrap_err();
        assert!(err.contains("10.103.0.1"), "{err}");
    }

    #[test]
    fn a_node_running_an_image_is_entered_through_its_infra_container() {
        // ecu2 runs a published image; its far side is refused above, so probe to ecu1's slice
        // peer is not possible either. Check the end alone.
        let model = crate::model::load_yaml(&format!(
            "{WS}measurements:\n  a: {{ kind: rtt-probe, from: ecu1, to: internet, slice: adas }}\n"
        ))
        .unwrap();
        let gateway = GatewayPlan::from_model(&model, crate::instance::Instance::FIRST).unwrap();
        let nodes =
            crate::zones::plan_nodes_from_model(&model, &gateway, Path::new("/nonexistent"))
                .unwrap();
        let e = end_on(&model, &nodes, "ecu2", "info", "measurement `a`").unwrap();
        assert!(e.image);
        assert_eq!(
            e.runtime_name,
            crate::zones::runtime_name_of(crate::instance::Instance::FIRST, "ecu2")
        );
    }

    #[test]
    fn a_probe_from_a_ue_reflects_in_the_ue_and_sends_from_the_host() {
        let ws = r#"
kind: vehicle
id: car
zones: { front: {} }
devices:
  ue1: { role: zone-controller, zone: front, backend: container }
functions:
  f1: { deployed_on: ue1 }
links:
  - { id: l1, endpoints: [ue1:eth0], slice: adas }
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
    - { id: adas, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }
measurements:
  ctl: { kind: rtt-probe, from: ue1, to: internet, slice: adas, rate_pps: 1000 }
"#;
        let model = crate::model::load_yaml(ws).unwrap();
        assert!(super::super::problems(&model).is_empty());
        let gateway = GatewayPlan::from_model(&model, crate::instance::Instance::FIRST).unwrap();
        let nodes =
            crate::zones::plan_nodes_from_model(&model, &gateway, Path::new("/nonexistent"))
                .unwrap();
        let p = plan(&model, &gateway, &nodes, Path::new("/nonexistent")).unwrap();
        assert_eq!(p[0].sender, Sender::DataNetwork { node: "ue1".into() });
        let Reflector::Ue(session) = &p[0].reflector else {
            panic!("{:?}", p[0].reflector)
        };
        assert_eq!(session.slice_id, "adas");
        assert_eq!(p[0].reflector.addr(), None);
    }

    /// exp-002's shape: the host probes each packaging of one ECU, a container and a guest.
    const ARMS: &str = r#"
kind: vehicle
id: car
zones: { front: {} }
devices:
  cont: { role: ecu, zone: front, backend: container }
  vm: { role: ecu, zone: front, backend: qemu-microvm }
functions:
  f-cont: { deployed_on: cont }
  f-vm: { deployed_on: vm }
links:
  - { id: a1, endpoints: [{ node: cont, port: eth0, ip: 10.101.0.11/24 }], slice: adas }
  - { id: a2, endpoints: [{ node: vm, port: eth0, ip: 10.101.0.12/24 }], slice: adas }
---
kind: baseline
id: bl
parts:
  - { ref: 'local/vxprobe:host-process', target_ids: f-cont }
  - { ref: ./guest, kind: bundle, target_ids: f-vm }
---
kind: experiment
id: exp
vehicles: [car]
baseline: bl
network:
  slices:
    - { id: adas, mode: vlan, vlan: 101 }
measurements:
  to-vm: { kind: rtt-probe, from: host, to: vm, slice: adas, rate_pps: 1000 }
  to-cont: { kind: rtt-probe, from: host, to: cont, slice: adas }
"#;

    #[test]
    fn a_probe_from_the_host_sends_through_the_far_side_of_its_targets_subnet() {
        let dir = tempfile::tempdir().unwrap();
        let model = crate::model::load_yaml(ARMS).unwrap();
        assert!(super::super::problems(&model).is_empty());
        let gateway = GatewayPlan::from_model(&model, crate::instance::Instance::FIRST).unwrap();
        let nodes = crate::zones::plan_nodes_from_model(&model, &gateway, dir.path()).unwrap();
        let p = plan(&model, &gateway, &nodes, Path::new("/nonexistent")).unwrap();
        let (cont, vm) = (&p[0], &p[1]);
        for q in [cont, vm] {
            let Sender::Host(f) = &q.sender else {
                panic!("{:?}", q.sender)
            };
            assert_eq!((f.vlan, f.cidr.as_str()), (101, "10.101.0.1/24"));
            assert_eq!(q.sender.node(), "host");
        }
        assert!(matches!(&cont.reflector, Reflector::Node(e) if e.addr == "10.101.0.11"));
        let Reflector::Guest { node, addr, meta } = &vm.reflector else {
            panic!("{:?}", vm.reflector)
        };
        assert_eq!((node.as_str(), addr.as_str()), ("vm", "10.101.0.12"));
        assert_eq!(meta, &dir.path().join("./guest").join(BUNDLE_META));
    }

    #[test]
    fn a_published_guests_reflector_is_read_from_the_part_cache() {
        let model = crate::model::load_yaml(&ARMS.replace(
            "{ ref: ./guest, kind: bundle, target_ids: f-vm }",
            "{ ref: ghcr.io/vxedge/parts/zone/linux/stack/probe-bundle, kind: bundle, digest: 'sha256:dce921197fa93f7a6bd306b1748b93785e329e443504bd2113a98ccfad52c5e6', target_ids: f-vm }",
        ))
        .unwrap();
        let gateway = GatewayPlan::from_model(&model, crate::instance::Instance::FIRST).unwrap();
        let nodes =
            crate::zones::plan_nodes_from_model(&model, &gateway, Path::new("/ws")).unwrap();
        let p = plan(&model, &gateway, &nodes, Path::new("/cache")).unwrap();
        let Reflector::Guest { meta, .. } = &p[1].reflector else {
            panic!("{:?}", p[1].reflector)
        };
        assert_eq!(
            meta,
            Path::new(
                "/cache/dce921197fa93f7a6bd306b1748b93785e329e443504bd2113a98ccfad52c5e6/meta.json"
            )
        );
    }

    #[test]
    fn a_guest_reflector_is_the_udp_port_its_part_declares() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().join(BUNDLE_META);
        assert!(
            declared_reflector(&meta)
                .unwrap_err()
                .contains("cannot read")
        );
        std::fs::write(&meta, r#"{"org.vxedge.reflector": "udp/9800"}"#).unwrap();
        assert_eq!(declared_reflector(&meta), Ok(9800));
        std::fs::write(&meta, r#"{"org.vxedge.arch": "x86_64"}"#).unwrap();
        let none = declared_reflector(&meta).unwrap_err();
        assert!(
            none.contains("declares no `org.vxedge.reflector`"),
            "{none}"
        );
        for bad in [r#""tcp/9800""#, r#""udp/0""#, r#""udp/x""#, "9800"] {
            std::fs::write(&meta, format!(r#"{{"org.vxedge.reflector": {bad}}}"#)).unwrap();
            let e = declared_reflector(&meta).unwrap_err();
            assert!(e.contains("not `udp/<port>`"), "{bad}: {e}");
        }
    }

    #[test]
    fn a_reflector_that_never_answers_is_not_waited_for_past_a_bad_address() {
        assert!(!wait_for_reflector("", "not-an-address", 9800));
    }

    #[test]
    fn first_host_of_a_subnet() {
        assert_eq!(first_host("10.101.0.2/24").as_deref(), Some("10.101.0.1"));
        assert_eq!(
            first_host("192.168.7.77/28").as_deref(),
            Some("192.168.7.65")
        );
        assert_eq!(first_host("10.0.0.1/32"), None);
        assert_eq!(first_host("fd00::1/64"), None);
    }

    #[test]
    fn samples_take_the_measurement_entity() {
        let text = "# TYPE x counter\nvxprobe_sent_total 5\nvxprobe_rtt_seconds{quantile=\"0.5\"} 0.001\nodd{} 1\n";
        assert_eq!(
            relabel(text, "measurements/a"),
            "vxprobe_sent_total{entity=\"measurements/a\"} 5\n\
             vxprobe_rtt_seconds{entity=\"measurements/a\",quantile=\"0.5\"} 0.001\n\
             odd{entity=\"measurements/a\"} 1\n"
        );
    }

    #[test]
    fn nothing_started_is_not_started_and_stop_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(status(dir.path(), "a"), State::NotStarted);
        assert!(started(dir.path()).is_empty());
        assert_eq!(collect_metrics(dir.path()), "");
        let sh = Shell::default();
        assert_eq!(stop(&sh, dir.path(), "a"), "measurement `a` stopped");
    }
}
