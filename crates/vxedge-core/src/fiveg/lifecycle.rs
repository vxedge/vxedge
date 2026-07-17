//! Podman-backed lifecycle for the containerized 5G core (track C, hybrid
//! policy §15). Same shape as `services.rs`'s process backend: idempotent
//! up/down, state observed rather than assumed. Where `services.rs` tracks
//! one pidfile per process, this tracks **two pods** (podman's shared-netns
//! container groups — `containers/fiveg/pod-up.sh` is the shell reference
//! this module replaces with a tested, typed Rust path).
//!
//! **Two pods, not one** (found necessary live, 2026-07-17): the RAN (gNB +
//! UEs) cannot share a network namespace with the CN (AMF + per-slice
//! SMF/UPF) — co-located, the UE's own `uesimtun<i>` and the UPF's
//! `vxtun<i>` install *identical* connected routes for the same UE subnet,
//! an ambiguity that never occurs in a real deployment (the UE is a
//! physically separate device). `fiveg_up` creates a CN pod and a RAN pod,
//! joins them with a veth pair on the transit subnet
//! ([`super::plan::TRANSIT_SUBNET`] — the N2/N3 equivalent), starts the gNB
//! in the RAN pod, and explicitly configures each UPF's tun device (Open5GS
//! creates the raw device but — standard bare-metal practice — leaves IP
//! configuration to the deployer; found live: it comes up `DOWN`, unrouted,
//! with nothing configuring it otherwise). Individual UEs are started
//! separately per test run against the RAN pod (the vxprobe/vxtraffic
//! pattern: infrastructure is provisioned here, traffic is driven by a
//! separate runner).
//!
//! Scope note: this module is the *mechanism* — bring a `FiveGPlan` up as
//! running pods, observe them, tear them down, and (via [`bridge_up`]/
//! [`bridge_down`]) attach the CN pod to an external bridge. It is not yet
//! wired to any vxAPI RPC or manifest lifecycle verb; that wiring
//! (GatewayService recognizing `mode: s-nssai` slices, and deciding
//! whether/how the bridge attachment joins `vxgateway`'s real, VLAN-aware
//! `vxbr0`) is tracked separately (`vxedge/TASKS.md` §4) since it is a
//! proto/RPC design decision, not a mechanism one — [`bridge_up`]
//! deliberately does not touch `vxbr0` or any VLAN membership itself.
//!
//! Two-layer split for testability (rust-patterns: pure logic vs. I/O):
//! [`core_run_specs`] (here) and [`super::subscriber::subscriber_insert_commands`]
//! are pure — they describe *what* to run, unit-tested without touching
//! podman. [`fiveg_up`] and friends are the thin, untested-except-by-the-
//! ignored-integration-test layer that actually execs `podman`/`ip`/`nsenter`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::plan::{CN_TRANSIT_ADDR, FiveGPlan, RAN_TRANSIT_ADDR, SliceSession};
use super::render::write_all;
use super::subscriber::subscriber_insert_commands;

#[derive(Debug, thiserror::Error)]
pub enum FiveGLifecycleError {
    #[error(
        "cannot execute `{program}`: {source} (is podman/iproute2/util-linux installed? see containers/fiveg/README.md)"
    )]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("`{cmdline}` failed: {stderr}")]
    Command { cmdline: String, stderr: String },
    #[error("cannot write rendered configs to {dir}: {source}")]
    Render { dir: String, source: std::io::Error },
    #[error("cannot parse `{cmdline}` output: {detail}")]
    Parse { cmdline: String, detail: String },
    #[error(
        "pod `{0}` has no running infra container yet (needs `pod start` before its netns can be joined)"
    )]
    NoInfraPid(String),
}

/// Which pods + images to drive. Defaults match `containers/fiveg/`.
#[derive(Debug, Clone)]
pub struct FiveGLifecycleConfig {
    /// CN pod name (mongo + 8-NF core + per-slice SMF/UPF).
    pub pod_name: String,
    pub core_image: String,
    /// RAN pod name (gNB + UEs). No implicit derivation from `pod_name` at
    /// construction time — set explicitly, or use [`Default::default`]
    /// which pairs `"vx5g"`/`"vx5g-ran"`.
    pub ran_pod_name: String,
    pub ran_image: String,
}

impl Default for FiveGLifecycleConfig {
    fn default() -> Self {
        let pod_name = "vx5g".to_string();
        Self {
            ran_pod_name: format!("{pod_name}-ran"),
            pod_name,
            core_image: "vx5g-core".to_string(),
            ran_image: "vx5g-ran".to_string(),
        }
    }
}

/// Interface names derived from the pod name, truncated to fit IFNAMSIZ
/// (15 usable chars) — see `gateway::plan`'s own note on the same limit.
fn transit_veth_names(pod_name: &str) -> (String, String) {
    let base: String = pod_name.chars().take(10).collect();
    (format!("{base}-cn0"), format!("{base}-ran0"))
}

/// Settle time between dependency phases. A fixed sleep, not a health poll —
/// matches `pod-up.sh`'s proven-live timings; polling a readiness signal
/// (e.g. NRF's SBI port) is a tracked hardening follow-up, not a correctness
/// gap in what's shipped today.
const PHASE_SETTLE: Duration = Duration::from_secs(2);

fn run(program: &str, args: &[String]) -> Result<String, FiveGLifecycleError> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|source| FiveGLifecycleError::Spawn {
            program: program.to_string(),
            source,
        })?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(FiveGLifecycleError::Command {
        cmdline: format!("{program} {}", args.join(" ")),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

fn podman(args: &[String]) -> Result<String, FiveGLifecycleError> {
    run("podman", args)
}

/// Run `ip <args>` directly on the host (for creating the transit veth pair
/// and moving each end into a target netns). Prefixed with `sudo`, not run
/// as the calling process's own UID: **podman itself must stay unprivileged**
/// — found live, 2026-07-17, running the whole test under `sudo` switched
/// `podman` to *root's* separate rootless-podman identity (its own
/// `/etc/containers` storage), which had never built `vx5g-core`/`vx5g-ran`
/// and refused to resolve the short image name at all. Host networking
/// genuinely needs CAP_NET_ADMIN (same requirement `netops::Shell` already
/// documents for gateway provisioning); podman does not — so only `ip` and
/// `nsenter` escalate, `podman` never does. Requires either a cached `sudo`
/// credential (`sudo -v` beforehand) or passwordless `sudo` for `ip`/
/// `nsenter` — the process itself must stay unprivileged for `podman` to
/// keep resolving the invoking user's own image storage.
fn host_ip(args: &[&str]) -> Result<String, FiveGLifecycleError> {
    let mut full = vec!["ip".to_string()];
    full.extend(args.iter().map(|s| s.to_string()));
    run("sudo", &full)
}

/// Run `ip <args>` inside the network namespace of the process at `pid`
/// (`sudo nsenter -t <pid> -n`) — for configuring an interface *after* it
/// has been moved into a pod's netns. Same sudo-scoping rationale as
/// [`host_ip`]: escalates only this call, never `podman` itself. Podman
/// pods aren't registered under `ip netns`'s named registry the way
/// `zones::backend`'s raw netns zones are, hence `nsenter` by PID rather
/// than `ip netns exec` by name.
fn nsenter_ip(pid: u32, args: &[&str]) -> Result<String, FiveGLifecycleError> {
    let mut full = vec![
        "nsenter".to_string(),
        "-t".to_string(),
        pid.to_string(),
        "-n".to_string(),
        "--".to_string(),
        "ip".to_string(),
    ];
    full.extend(args.iter().map(|s| s.to_string()));
    run("sudo", &full)
}

/// PID of a pod's infra container — the anchor for its network namespace.
/// Requires the pod to have been started (`pod start`/`pod create` alone
/// leaves the infra container's PID at 0 — found live, 2026-07-17).
pub fn pod_pid(pod_name: &str) -> Result<u32, FiveGLifecycleError> {
    let infra_id = podman(&[
        "pod".to_string(),
        "inspect".to_string(),
        pod_name.to_string(),
        "--format".to_string(),
        "{{.InfraContainerID}}".to_string(),
    ])?;
    let pid_str = podman(&[
        "inspect".to_string(),
        infra_id.trim().to_string(),
        "--format".to_string(),
        "{{.State.Pid}}".to_string(),
    ])?;
    let pid: u32 = pid_str
        .trim()
        .parse()
        .map_err(|_| FiveGLifecycleError::Parse {
            cmdline: format!(
                "podman inspect {} --format {{{{.State.Pid}}}}",
                infra_id.trim()
            ),
            detail: format!("not a PID: {pid_str:?}"),
        })?;
    if pid == 0 {
        return Err(FiveGLifecycleError::NoInfraPid(pod_name.to_string()));
    }
    Ok(pid)
}

/// True if the pod is up (any state) — the idempotency check for `up`, and
/// the fast path for `status`/`down` on an already-absent pod.
pub fn pod_exists(pod_name: &str) -> Result<bool, FiveGLifecycleError> {
    match Command::new("podman")
        .args(["pod", "exists", pod_name])
        .status()
    {
        Ok(status) => Ok(status.success()),
        Err(source) => Err(FiveGLifecycleError::Spawn {
            program: "podman".to_string(),
            source,
        }),
    }
}

/// One container to bring up inside the pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSpec {
    /// Container name suffix; full name is `<pod>-<name>`.
    pub name: String,
    pub image: String,
    /// Extra `podman run` flags before the image (e.g. capabilities/devices).
    pub extra_flags: Vec<String>,
    /// Command + args run inside the container.
    pub cmd: Vec<String>,
}

/// The 8-NF Open5GS core + per-slice SMF/UPF, grouped into dependency
/// phases: mongo → NRF → the SBI-plane NFs (independent NRF clients, any
/// order within the phase) → AMF → per-slice SMF → per-slice UPF (needs
/// NET_ADMIN + /dev/net/tun for its vxtun device). Pure — no podman calls —
/// so the ordering and per-container flags are unit-testable directly.
pub fn core_run_specs(plan: &FiveGPlan, core_image: &str) -> Vec<Vec<RunSpec>> {
    let nf = |name: &str| RunSpec {
        name: name.to_string(),
        image: core_image.to_string(),
        extra_flags: vec![],
        cmd: vec![
            format!("open5gs-{name}d"),
            "-c".to_string(),
            format!("/etc/vx5g/open5gs/{name}.yaml"),
        ],
    };

    vec![
        vec![RunSpec {
            name: "mongo".to_string(),
            image: "docker.io/library/mongo:7".to_string(),
            extra_flags: vec![],
            cmd: vec![
                "mongod".to_string(),
                "--bind_ip".to_string(),
                "127.0.0.1".to_string(),
                "--quiet".to_string(),
            ],
        }],
        vec![nf("nrf")],
        vec![
            nf("ausf"),
            nf("udm"),
            nf("udr"),
            nf("pcf"),
            nf("bsf"),
            nf("nssf"),
        ],
        vec![nf("amf")],
        plan.sessions
            .iter()
            .map(|s| RunSpec {
                name: format!("smf-{}", s.slice_id),
                image: core_image.to_string(),
                extra_flags: vec![],
                cmd: vec![
                    "open5gs-smfd".to_string(),
                    "-c".to_string(),
                    format!("/etc/vx5g/open5gs/smf-{}.yaml", s.slice_id),
                ],
            })
            .collect(),
        plan.sessions
            .iter()
            .map(|s| RunSpec {
                name: format!("upf-{}", s.slice_id),
                image: core_image.to_string(),
                extra_flags: vec![
                    "--cap-add=NET_ADMIN".to_string(),
                    "--device".to_string(),
                    "/dev/net/tun".to_string(),
                ],
                cmd: vec![
                    "open5gs-upfd".to_string(),
                    "-c".to_string(),
                    format!("/etc/vx5g/open5gs/upf-{}.yaml", s.slice_id),
                ],
            })
            .collect(),
    ]
}

fn run_detached(pod: &str, cfg_dir: &Path, spec: &RunSpec) -> Result<(), FiveGLifecycleError> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--pod".into(),
        pod.into(),
        "--name".into(),
        format!("{pod}-{}", spec.name),
        "-v".into(),
        format!("{}:/etc/vx5g:ro,Z", cfg_dir.display()),
    ];
    args.extend(spec.extra_flags.iter().cloned());
    args.push(spec.image.clone());
    args.extend(spec.cmd.iter().cloned());
    podman(&args)?;
    Ok(())
}

/// Join the CN and RAN pods with a veth pair on `TRANSIT_SUBNET` — the
/// mechanism side of the N2/N3 equivalent. Both ends are created on the
/// host, then each is moved into its target pod's netns and addressed
/// (`nsenter`, since podman pods aren't in `ip netns`'s named registry).
fn setup_transit_link(
    cfg: &FiveGLifecycleConfig,
    cn_pid: u32,
    ran_pid: u32,
) -> Result<(), FiveGLifecycleError> {
    let (cn_side, ran_side) = transit_veth_names(&cfg.pod_name);
    host_ip(&[
        "link", "add", &cn_side, "type", "veth", "peer", "name", &ran_side,
    ])?;
    host_ip(&["link", "set", &cn_side, "netns", &cn_pid.to_string()])?;
    host_ip(&["link", "set", &ran_side, "netns", &ran_pid.to_string()])?;

    let cn_addr = format!("{CN_TRANSIT_ADDR}/24");
    nsenter_ip(cn_pid, &["addr", "add", &cn_addr, "dev", &cn_side])?;
    nsenter_ip(cn_pid, &["link", "set", &cn_side, "up"])?;

    let ran_addr = format!("{RAN_TRANSIT_ADDR}/24");
    nsenter_ip(ran_pid, &["addr", "add", &ran_addr, "dev", &ran_side])?;
    nsenter_ip(ran_pid, &["link", "set", &ran_side, "up"])?;
    Ok(())
}

/// Configure a slice's UPF tun device — Open5GS creates the raw device but
/// leaves IP configuration to the deployer (standard bare-metal practice;
/// found live, 2026-07-17: it otherwise stays `DOWN`, no route, and nothing
/// else configures it). Assigning `ue_gateway` here is what makes
/// `ue_subnet` a connected route in the CN pod's netns, which is how
/// downlink traffic to a UE's IP finds its way back into the UPF's GTP-U
/// tunnel.
fn setup_upf_tun(cn_pid: u32, session: &SliceSession) -> Result<(), FiveGLifecycleError> {
    let addr = format!("{}/16", session.ue_gateway);
    nsenter_ip(cn_pid, &["addr", "add", &addr, "dev", &session.tun_dev])?;
    nsenter_ip(cn_pid, &["link", "set", &session.tun_dev, "up"])?;
    Ok(())
}

/// Address for the CN pod's side of [`bridge_up`]'s attachment — distinct
/// from the CN↔RAN transit subnet, since this is a different link (CN pod
/// ↔ bridge, not CN pod ↔ RAN pod).
pub const BRIDGE_ATTACH_SUBNET: &str = "10.98.0.0/24";
pub const BRIDGE_ATTACH_CN_ADDR: &str = "10.98.0.1";

fn bridge_veth_names(pod_name: &str) -> (String, String) {
    let base: String = pod_name.chars().take(10).collect();
    (format!("{base}-br0"), format!("{base}-brp0"))
}

/// Attach the CN pod's netns to a bridge as a plain access port (no VLAN
/// tagging — a deliberately minimal mechanism spike, 2026-07-17, proving
/// that the fiveg CN pod's UE-subnet routing is reachable from *outside*
/// its own netns at all, through the exact same veth-into-bridge primitive
/// `gateway::provision::apply_endpoints` already uses for zone nodes).
///
/// **Not `vxgateway`'s real bridge, and not wired into `fiveg_up`.** Uses
/// whatever bridge name is passed in — pass a throwaway test bridge, not
/// `vxbr0`, unless you have deliberately decided to attach to the real
/// one (`vxbr0` is `vlan_filtering 1`; a port needs an explicit `bridge
/// vlan add ... pvid untagged` to pass any traffic on such a bridge, which
/// this function does not do — that VLAN-membership decision belongs to
/// the real GatewayService integration, tracked separately,
/// `vxedge/TASKS.md` §4, not decided as a side effect here).
pub fn bridge_up(
    cfg: &FiveGLifecycleConfig,
    bridge: &str,
    cn_pid: u32,
) -> Result<(), FiveGLifecycleError> {
    let (host_side, pod_side) = bridge_veth_names(&cfg.pod_name);
    host_ip(&[
        "link", "add", &host_side, "type", "veth", "peer", "name", &pod_side,
    ])?;
    host_ip(&["link", "set", &pod_side, "netns", &cn_pid.to_string()])?;

    host_ip(&["link", "set", &host_side, "master", bridge])?;
    host_ip(&["link", "set", &host_side, "up"])?;

    let cn_addr = format!("{BRIDGE_ATTACH_CN_ADDR}/24");
    nsenter_ip(cn_pid, &["addr", "add", &cn_addr, "dev", &pod_side])?;
    nsenter_ip(cn_pid, &["link", "set", &pod_side, "up"])?;
    Ok(())
}

/// Remove the bridge attachment. Idempotent: deleting the host-side veth
/// also destroys its pod-side peer, wherever it currently lives; a missing
/// interface is tolerated (`ip link del` failing with "Cannot find device"
/// means it's already gone, not an error worth surfacing).
pub fn bridge_down(cfg: &FiveGLifecycleConfig) -> Result<(), FiveGLifecycleError> {
    let (host_side, _) = bridge_veth_names(&cfg.pod_name);
    match host_ip(&["link", "del", &host_side]) {
        Ok(_) => Ok(()),
        Err(FiveGLifecycleError::Command { stderr, .. })
            if stderr.contains("Cannot find device") || stderr.contains("does not exist") =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Bring the 5G core up as a running pod. Idempotent: an already-existing
/// pod is left untouched (matches `services.rs::svc_up`'s convention).
pub fn fiveg_up(
    plan: &FiveGPlan,
    runtime_dir: &Path,
    cfg: &FiveGLifecycleConfig,
) -> Result<String, FiveGLifecycleError> {
    if pod_exists(&cfg.pod_name)? {
        return Ok(format!("pod `{}` already up (idempotent)", cfg.pod_name));
    }

    let cfg_dir = runtime_dir.join("fiveg-config");
    write_all(plan, &cfg_dir).map_err(|source| FiveGLifecycleError::Render {
        dir: cfg_dir.display().to_string(),
        source,
    })?;

    // CN pod: the 8-NF core + per-slice SMF/UPF, in dependency order.
    podman(&[
        "pod".into(),
        "create".into(),
        "--name".into(),
        cfg.pod_name.clone(),
    ])?;
    podman(&["pod".into(), "start".into(), cfg.pod_name.clone()])?;

    let phases = core_run_specs(plan, &cfg.core_image);
    let phase_count = phases.len();
    // Split before "amf" (phase index 3 — after [mongo],[nrf],[sbi-6]):
    // AMF and every per-slice UPF are deliberately started *after* the
    // transit link exists, not before. Found live (2026-07-17), in two
    // rounds: first UPF's GTP-U bind failed (its `upf_gtpu_addr` is now a
    // transit-subnet address, not loopback, and didn't exist on any
    // interface yet); fixing only that surfaced the identical bug in AMF
    // (its NGAP address, `amf_ngap_addr`, is the transit link's own
    // primary address — same problem, same fix). Both bind targets must
    // exist before their daemons start, or the process exits immediately
    // and (for UPF) its tun device never comes up. mongo/nrf/the 6
    // SBI-plane NFs and SMF stay loopback-only and are unaffected.
    const TRANSIT_DEPENDENT_PHASES_FROM: usize = 3;
    let (pre_transit_phases, post_transit_phases) = phases
        .split_at_checked(TRANSIT_DEPENDENT_PHASES_FROM)
        .expect("core_run_specs always returns at least 4 phases");
    for phase in pre_transit_phases {
        for spec in phase {
            run_detached(&cfg.pod_name, &cfg_dir, spec)?;
        }
        std::thread::sleep(PHASE_SETTLE);
    }

    // RAN pod: gNB only — individual UEs are started per test run against
    // it (the vxprobe/vxtraffic pattern: provisioning here, traffic
    // driven separately).
    podman(&[
        "pod".into(),
        "create".into(),
        "--name".into(),
        cfg.ran_pod_name.clone(),
    ])?;
    podman(&["pod".into(), "start".into(), cfg.ran_pod_name.clone()])?;

    // Join the two pods (assigns CN_TRANSIT_ADDR — what AMF's NGAP server
    // binds to), then give each slice's UPF-to-be its own GTP-U address on
    // the CN side of the link too — everything the phases below need must
    // exist before they start.
    let cn_pid = pod_pid(&cfg.pod_name)?;
    let ran_pid = pod_pid(&cfg.ran_pod_name)?;
    setup_transit_link(cfg, cn_pid, ran_pid)?;
    let (cn_transit_dev, _) = transit_veth_names(&cfg.pod_name);
    for session in &plan.sessions {
        nsenter_ip(
            cn_pid,
            &[
                "addr",
                "add",
                &format!("{}/32", session.upf_gtpu_addr),
                "dev",
                &cn_transit_dev,
            ],
        )?;
    }

    // Now amf, smf, and upf — their transit-subnet bind addresses all
    // exist, so every daemon starts and stays up this time.
    for phase in post_transit_phases {
        for spec in phase {
            run_detached(&cfg.pod_name, &cfg_dir, spec)?;
        }
        std::thread::sleep(PHASE_SETTLE);
    }
    for session in &plan.sessions {
        setup_upf_tun(cn_pid, session)?;
    }

    run_detached(
        &cfg.ran_pod_name,
        &cfg_dir,
        &RunSpec {
            name: "gnb".to_string(),
            image: cfg.ran_image.clone(),
            extra_flags: vec![],
            cmd: vec![
                "nr-gnb".to_string(),
                "-c".to_string(),
                "/etc/vx5g/ueransim/gnb.yaml".to_string(),
            ],
        },
    )?;
    std::thread::sleep(PHASE_SETTLE);

    for cmd in subscriber_insert_commands(plan) {
        let mut args: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "--pod".into(),
            cfg.pod_name.clone(),
            cfg.core_image.clone(),
        ];
        args.extend(cmd);
        podman(&args)?;
    }

    Ok(format!(
        "CN pod `{}` up ({} dependency phases) + RAN pod `{}` up (gNB), transit link {}<->{}, {} subscribers provisioned",
        cfg.pod_name,
        phase_count,
        cfg.ran_pod_name,
        CN_TRANSIT_ADDR,
        RAN_TRANSIT_ADDR,
        plan.sessions.len()
    ))
}

/// Tear both pods down. Idempotent: an already-absent pod is a no-op
/// (`podman pod rm --ignore`). The transit veth is destroyed automatically
/// when either end's netns is destroyed — no explicit cleanup needed.
pub fn fiveg_down(cfg: &FiveGLifecycleConfig) -> Result<String, FiveGLifecycleError> {
    podman(&[
        "pod".into(),
        "rm".into(),
        "-f".into(),
        "--ignore".into(),
        cfg.ran_pod_name.clone(),
    ])?;
    podman(&[
        "pod".into(),
        "rm".into(),
        "-f".into(),
        "--ignore".into(),
        cfg.pod_name.clone(),
    ])?;
    Ok(format!(
        "pods `{}` and `{}` down",
        cfg.pod_name, cfg.ran_pod_name
    ))
}

#[derive(Debug, Clone, serde::Deserialize)]
struct PsEntry {
    #[serde(rename = "IsInfra")]
    is_infra: bool,
    #[serde(rename = "Names")]
    names: Vec<String>,
    #[serde(rename = "State")]
    state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerStatus {
    pub name: String,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct FiveGStatus {
    pub pod_name: String,
    pub up: bool,
    pub containers: Vec<ContainerStatus>,
    pub ran_pod_name: String,
    pub ran_up: bool,
    pub ran_containers: Vec<ContainerStatus>,
}

impl FiveGStatus {
    /// Every CN and RAN container (excluding each pod's infra/pause
    /// container) reports `running`.
    pub fn all_running(&self) -> bool {
        self.up
            && self.ran_up
            && !self.containers.is_empty()
            && !self.ran_containers.is_empty()
            && self
                .containers
                .iter()
                .chain(self.ran_containers.iter())
                .all(|c| c.state == "running")
    }
}

fn pod_containers(pod_name: &str) -> Result<(bool, Vec<ContainerStatus>), FiveGLifecycleError> {
    if !pod_exists(pod_name)? {
        return Ok((false, vec![]));
    }
    let out = podman(&[
        "ps".into(),
        "--pod".into(),
        "--filter".into(),
        format!("pod={pod_name}"),
        "--format".into(),
        "json".into(),
    ])?;
    let entries: Vec<PsEntry> =
        serde_json::from_str(&out).map_err(|e| FiveGLifecycleError::Parse {
            cmdline: format!("podman ps --pod --filter pod={pod_name} --format json"),
            detail: e.to_string(),
        })?;
    let containers = entries
        .into_iter()
        .filter(|e| !e.is_infra)
        .map(|e| ContainerStatus {
            name: e.names.into_iter().next().unwrap_or_default(),
            state: e.state,
        })
        .collect();
    Ok((true, containers))
}

/// Observe both pods' actual state — the reconciler's read side, mirroring
/// `gateway::provision::observe`.
pub fn fiveg_status(cfg: &FiveGLifecycleConfig) -> Result<FiveGStatus, FiveGLifecycleError> {
    let (up, containers) = pod_containers(&cfg.pod_name)?;
    let (ran_up, ran_containers) = pod_containers(&cfg.ran_pod_name)?;
    Ok(FiveGStatus {
        pod_name: cfg.pod_name.clone(),
        up,
        containers,
        ran_pod_name: cfg.ran_pod_name.clone(),
        ran_up,
        ran_containers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ExperimentManifest;

    fn plan_two_slices() -> FiveGPlan {
        let yaml = r#"
experiment: { id: t, name: t, duration_s: 1, results_prefix: r }
network:
  slices:
    - { id: adas, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }
    - { id: infotainment, mode: s-nssai, s_nssai: { sst: 2 } }
"#;
        let m = ExperimentManifest::from_yaml(yaml).expect("test manifest parses");
        FiveGPlan::from_manifest(&m).expect("plans")
    }

    #[test]
    fn phases_are_ordered_mongo_nrf_sbi_amf_smf_upf() {
        let phases = core_run_specs(&plan_two_slices(), "vx5g-core");
        assert_eq!(phases.len(), 6);
        assert_eq!(phases[0].len(), 1);
        assert_eq!(phases[0][0].name, "mongo");
        assert_eq!(phases[1][0].name, "nrf");
        let sbi_names: Vec<&str> = phases[2].iter().map(|s| s.name.as_str()).collect();
        assert_eq!(sbi_names, ["ausf", "udm", "udr", "pcf", "bsf", "nssf"]);
        assert_eq!(phases[3][0].name, "amf");
        // one smf/upf per slice, in manifest order
        assert_eq!(phases[4].len(), 2);
        assert_eq!(phases[4][0].name, "smf-adas");
        assert_eq!(phases[4][1].name, "smf-infotainment");
        assert_eq!(phases[5][0].name, "upf-adas");
    }

    #[test]
    fn upf_specs_carry_net_admin_and_tun_device_others_dont() {
        let phases = core_run_specs(&plan_two_slices(), "vx5g-core");
        let upf_phase = phases.last().unwrap();
        for spec in upf_phase {
            assert!(
                spec.extra_flags
                    .contains(&"--cap-add=NET_ADMIN".to_string())
            );
            assert!(spec.extra_flags.contains(&"/dev/net/tun".to_string()));
        }
        // mongo, nrf, and the SBI-plane NFs need neither.
        for phase in &phases[..4] {
            for spec in phase {
                assert!(
                    spec.extra_flags.is_empty(),
                    "{} should have no extra flags",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn nf_configs_point_at_their_own_rendered_file() {
        let phases = core_run_specs(&plan_two_slices(), "vx5g-core");
        assert_eq!(
            phases[1][0].cmd,
            vec!["open5gs-nrfd", "-c", "/etc/vx5g/open5gs/nrf.yaml"]
        );
        assert_eq!(
            phases[4][0].cmd,
            vec!["open5gs-smfd", "-c", "/etc/vx5g/open5gs/smf-adas.yaml"]
        );
    }

    // Subscriber-command tests moved to subscriber.rs (2026-07-17): the
    // dbctl-based codepath they tested is gone, replaced by direct MongoDB
    // document writes that fix a real bug (dbctl's SD-less fallback
    // hardcoded sst=1, ignoring the slice's actual SST).

    #[test]
    fn default_config_matches_the_containers_fiveg_images() {
        let cfg = FiveGLifecycleConfig::default();
        assert_eq!(cfg.pod_name, "vx5g");
        assert_eq!(cfg.core_image, "vx5g-core");
        assert_eq!(cfg.ran_pod_name, "vx5g-ran");
        assert_eq!(cfg.ran_image, "vx5g-ran");
    }

    fn nrf_running() -> ContainerStatus {
        ContainerStatus {
            name: "vx5g-nrf".into(),
            state: "running".into(),
        }
    }
    fn gnb_running() -> ContainerStatus {
        ContainerStatus {
            name: "vx5g-ran-gnb".into(),
            state: "running".into(),
        }
    }

    #[test]
    fn all_running_is_false_when_either_pod_down() {
        let cn_down = FiveGStatus {
            pod_name: "vx5g".into(),
            up: false,
            containers: vec![],
            ran_pod_name: "vx5g-ran".into(),
            ran_up: true,
            ran_containers: vec![gnb_running()],
        };
        assert!(!cn_down.all_running());

        let ran_down = FiveGStatus {
            pod_name: "vx5g".into(),
            up: true,
            containers: vec![nrf_running()],
            ran_pod_name: "vx5g-ran".into(),
            ran_up: false,
            ran_containers: vec![],
        };
        assert!(!ran_down.all_running());
    }

    #[test]
    fn all_running_requires_every_container_in_both_pods_running() {
        let s = FiveGStatus {
            pod_name: "vx5g".into(),
            up: true,
            containers: vec![
                nrf_running(),
                ContainerStatus {
                    name: "vx5g-amf".into(),
                    state: "exited".into(),
                },
            ],
            ran_pod_name: "vx5g-ran".into(),
            ran_up: true,
            ran_containers: vec![gnb_running()],
        };
        assert!(!s.all_running());
    }

    #[test]
    fn all_running_true_when_every_container_in_both_pods_running() {
        let s = FiveGStatus {
            pod_name: "vx5g".into(),
            up: true,
            containers: vec![nrf_running()],
            ran_pod_name: "vx5g-ran".into(),
            ran_up: true,
            ran_containers: vec![gnb_running()],
        };
        assert!(s.all_running());
    }

    #[test]
    fn transit_veth_names_fit_ifnamsiz_and_are_derived_from_pod_name() {
        let (cn, ran) = transit_veth_names("vx5g");
        assert_eq!(cn, "vx5g-cn0");
        assert_eq!(ran, "vx5g-ran0");
        assert!(cn.len() <= 15 && ran.len() <= 15);

        let (cn, ran) = transit_veth_names("a-very-long-custom-pod-name");
        assert!(cn.len() <= 15, "{cn} exceeds IFNAMSIZ");
        assert!(ran.len() <= 15, "{ran} exceeds IFNAMSIZ");
    }

    #[test]
    fn bridge_veth_names_fit_ifnamsiz_and_differ_from_transit_names() {
        let (host, pod) = bridge_veth_names("vx5g");
        assert_eq!(host, "vx5g-br0");
        assert_eq!(pod, "vx5g-brp0");
        assert!(host.len() <= 15 && pod.len() <= 15);

        // Distinct namespaces of names: bridge attach vs CN<->RAN transit
        // link must never collide even for the same pod_name.
        let (cn, ran) = transit_veth_names("vx5g");
        assert_ne!(host, cn);
        assert_ne!(host, ran);
        assert_ne!(pod, cn);
        assert_ne!(pod, ran);

        let (host, pod) = bridge_veth_names("a-very-long-custom-pod-name");
        assert!(host.len() <= 15, "{host} exceeds IFNAMSIZ");
        assert!(pod.len() <= 15, "{pod} exceeds IFNAMSIZ");
    }

    #[test]
    fn bridge_attach_addressing_is_distinct_from_transit_subnet() {
        assert_ne!(BRIDGE_ATTACH_SUBNET, super::super::plan::TRANSIT_SUBNET);
        assert_eq!(BRIDGE_ATTACH_CN_ADDR, "10.98.0.1");
    }
}
