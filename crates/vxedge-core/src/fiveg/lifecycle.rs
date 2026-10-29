//! Podman-backed lifecycle for the containerized 5G core (track C, hybrid
//! policy §15). Same shape as `services.rs`'s process backend: idempotent
//! up/down, state observed rather than assumed. Where `services.rs` tracks
//! one pidfile per process, this tracks **two pods**: shared-netns
//! container groups, driven through [`crate::containers`] over the Podman
//! API (Track V2; until then the `podman` CLI — `containers/fiveg/pod-up.sh`
//! is the shell reference both replaced). "Pod" below means one such group:
//! an infra container ([`FiveGLifecycleConfig::infra_image`]) owning a
//! network namespace that every member joins, containers named
//! `<pod>-<member>` as before.
//!
//! **Two pods plus one pod per UE** (found necessary live, 2026-07-17, in
//! two rounds). Round one: the RAN (gNB) cannot share a network namespace
//! with the CN (AMF + per-slice SMF/UPF) — co-located, the UE's own
//! `uesimtun<i>` and the UPF's `vxtun<i>` install *identical* connected
//! routes for the same UE subnet, an ambiguity that never occurs in a real
//! deployment (the UE is a physically separate device). `fiveg_up` creates
//! a CN pod and a RAN pod, joins them with a veth pair on the transit
//! subnet ([`super::plan::TRANSIT_SUBNET`] — the N2/N3 equivalent), starts
//! the gNB in the RAN pod, and explicitly configures each UPF's tun device
//! (Open5GS creates the raw device but — standard bare-metal practice —
//! leaves IP configuration to the deployer; found live: it comes up
//! `DOWN`, unrouted, with nothing configuring it otherwise). Round two,
//! found live during the bridging proof: multiple UEs sharing the RAN
//! pod's netns also collide — UERANSIM hardcodes routing table `1000` for
//! its per-UE default route rather than deriving it from the interface, so
//! a second UE's route insert fails against the first's. Fixed the same
//! way as round one: each UE gets its **own** pod ([`ue_up`]/[`ue_down`]),
//! joined to gNB via a small bridge entirely internal to the RAN pod's own
//! netns ([`ran_bridge_name`]) rather than a shared netns. Individual UEs
//! are still started separately per test run, not by `fiveg_up` itself
//! (the vxprobe/vxtraffic pattern: infrastructure is provisioned here,
//! traffic is driven by a separate runner) — `fiveg_up` only brings up the
//! CN pod and the (now UE-less) RAN pod running gNB alone.
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
//! are pure — they describe *what* to run, unit-tested without a container
//! engine. [`fiveg_up`] and friends are the thin, untested-except-by-the-
//! ignored-integration-test layer that drives the [`ContainerClient`] and
//! execs `ip`/`nsenter`.
//!
//! Async throughout, and so is every host command here (`tokio::process`,
//! `tokio::time::sleep`): these run directly on the daemon's runtime, not in
//! `spawn_blocking`, so nothing in them may block a worker thread.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

use super::plan::{CN_TRANSIT_ADDR, FiveGPlan, RAN_TRANSIT_ADDR, SliceSession};
use super::render::write_all;
use super::subscriber::subscriber_insert_commands;
use crate::containers::{Bind, ContainerClient, ContainerError, ContainerSpec, GroupSpec};

#[derive(Debug, thiserror::Error)]
pub enum FiveGLifecycleError {
    #[error(
        "cannot execute `{program}`: {source} (is sudo/iproute2/util-linux installed? see docs/TESTBED-SETUP.md)"
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
    #[error("{op}: {source}")]
    /// Boxed: the container error carries the API client's own, which is
    /// large, and this enum is returned on every call here.
    Containers {
        op: String,
        #[source]
        source: Box<ContainerError>,
    },
    #[error(
        "found {containers}, left by an older vxedged that drove the podman CLI: they carry no \
         `io.vxedge.group` label, so this version can neither reuse nor remove them. Remove the \
         old pods once, then re-run: `podman pod rm -f {pods}`"
    )]
    LegacyPods { containers: String, pods: String },
}

fn containers(op: impl Into<String>) -> impl FnOnce(ContainerError) -> FiveGLifecycleError {
    let op = op.into();
    move |source| FiveGLifecycleError::Containers {
        op,
        source: Box::new(source),
    }
}

pub use crate::containers::PAUSE_IMAGE;

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
    /// Image of every pod's namespace-owning infra container, digest-pinned
    /// ([`PAUSE_IMAGE`] by default).
    pub infra_image: String,
}

impl Default for FiveGLifecycleConfig {
    fn default() -> Self {
        let pod_name = "vx5g".to_string();
        Self {
            ran_pod_name: format!("{pod_name}-ran"),
            pod_name,
            core_image: "vx5g-core".to_string(),
            ran_image: "vx5g-ran".to_string(),
            infra_image: PAUSE_IMAGE.to_string(),
        }
    }
}

/// The group a pod name stands for.
fn pod_group(cfg: &FiveGLifecycleConfig, pod: &str) -> Result<GroupSpec, FiveGLifecycleError> {
    GroupSpec::new(
        pod,
        ContainerSpec {
            image: cfg.infra_image.clone(),
            ..Default::default()
        },
    )
    .map_err(|e| containers(format!("pod `{pod}`"))(e.into()))
}

/// The rendered config set, mounted read-only into every long-running
/// container (`-v <cfg>:/etc/vx5g:ro,Z`, as the CLI path passed it).
fn config_bind(cfg_dir: &Path) -> Bind {
    Bind {
        host: cfg_dir.to_path_buf(),
        container: "/etc/vx5g".to_string(),
        read_only: true,
        relabel: true,
    }
}

/// Fail with the migration command if any of `names` is held by a container
/// the podman-CLI code created ([`ContainerClient::unlabelled`]): label-based
/// `down` cannot see those, and `up` would stop halfway on a name clash.
async fn refuse_legacy(
    client: &ContainerClient,
    names: Vec<String>,
    pods: String,
) -> Result<(), FiveGLifecycleError> {
    let found = client
        .unlabelled(&names)
        .await
        .map_err(containers("look for containers left by the podman CLI"))?;
    if found.is_empty() {
        return Ok(());
    }
    Err(FiveGLifecycleError::LegacyPods {
        containers: found
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", "),
        pods,
    })
}

/// Interface names derived from the pod name, truncated to fit IFNAMSIZ
/// (15 usable chars) — see `gateway::plan`'s own note on the same limit.
fn transit_veth_names(pod_name: &str) -> (String, String) {
    let base: String = pod_name.chars().take(10).collect();
    (format!("{base}-cn0"), format!("{base}-ran0"))
}

/// Bridge inside the RAN pod's own netns that unifies its two kinds of
/// members: the transit veth's RAN-side peer (reaching the CN pod) and one
/// veth per UE pod (reaching each UE). `RAN_TRANSIT_ADDR` lives on this
/// bridge rather than directly on the transit veth, so it stays reachable
/// from both. Entirely internal to fiveg — never touches the host or
/// `vxgateway`'s own bridge.
fn ran_bridge_name(ran_pod_name: &str) -> String {
    let base: String = ran_pod_name.chars().take(10).collect();
    format!("{base}-br")
}

/// Per-UE veth pair names, session-index-keyed (not slice-id-keyed — slice
/// ids are unbounded-length manifest strings, unsafe to fit in IFNAMSIZ).
fn ue_veth_names(ran_pod_name: &str, index: usize) -> (String, String) {
    let base: String = ran_pod_name.chars().take(6).collect();
    (format!("{base}-ue{index}a"), format!("{base}-ue{index}b"))
}

/// Per-UE pod name — unlike interface names, pod names aren't IFNAMSIZ-
/// constrained, so this uses the slice id directly (readable, debuggable).
pub fn ue_pod_name(ran_pod_name: &str, slice_id: &str) -> String {
    format!("{ran_pod_name}-ue-{slice_id}")
}

/// Settle time between dependency phases. A fixed sleep, not a health poll —
/// matches `pod-up.sh`'s proven-live timings; polling a readiness signal
/// (e.g. NRF's SBI port) is a tracked hardening follow-up, not a correctness
/// gap in what's shipped today.
const PHASE_SETTLE: Duration = Duration::from_secs(2);

async fn run(program: &str, args: &[String]) -> Result<String, FiveGLifecycleError> {
    let out = Command::new(program)
        .args(args)
        .output()
        .await
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
/// keep resolving the invoking user's own image storage. (Since V2 podman is
/// reached over the user's API socket, not exec'd, but the identity rule is
/// the same: [`crate::containers`] refuses root's socket.)
async fn host_ip(args: &[&str]) -> Result<String, FiveGLifecycleError> {
    let mut full = vec!["ip".to_string()];
    full.extend(args.iter().map(|s| s.to_string()));
    run("sudo", &full).await
}

/// Run `<program> <args>` inside the *network* namespace of the process at
/// `pid` (`sudo nsenter -t <pid> -n`) — for configuring an interface, or
/// flipping a per-netns sysctl, *after* it has been moved into a pod's
/// netns. Same sudo-scoping rationale as [`host_ip`]: escalates only this
/// call, never `podman` itself. Podman pods aren't registered under
/// `ip netns`'s named registry the way `zones::backend`'s raw netns zones
/// are, hence `nsenter` by PID rather than `ip netns exec` by name.
async fn nsenter_run(
    pid: u32,
    program: &str,
    args: &[&str],
) -> Result<String, FiveGLifecycleError> {
    let mut full = vec![
        "nsenter".to_string(),
        "-t".to_string(),
        pid.to_string(),
        "-n".to_string(),
        "--".to_string(),
        program.to_string(),
    ];
    full.extend(args.iter().map(|s| s.to_string()));
    run("sudo", &full).await
}

async fn nsenter_ip(pid: u32, args: &[&str]) -> Result<String, FiveGLifecycleError> {
    nsenter_run(pid, "ip", args).await
}

/// PID of a pod's infra container — the anchor for its network namespace.
/// Fails with the group's own error ([`ContainerError::GroupNotUp`] or
/// [`ContainerError::NoInfraPid`]) when the pod is absent or its infra
/// container is not running.
pub async fn pod_pid(client: &ContainerClient, pod_name: &str) -> Result<u32, FiveGLifecycleError> {
    client
        .handle(pod_name)
        .await
        .map(|h| h.infra_pid)
        .map_err(containers(format!(
            "find pod `{pod_name}`'s network namespace"
        )))
}

/// True if the pod is up (any state) — the idempotency check for `up`, and
/// the fast path for `status`/`down` on an already-absent pod.
pub async fn pod_exists(
    client: &ContainerClient,
    pod_name: &str,
) -> Result<bool, FiveGLifecycleError> {
    client
        .group_status(pod_name)
        .await
        .map(|s| s.exists())
        .map_err(containers(format!("observe pod `{pod_name}`")))
}

/// The 8-NF Open5GS core + per-slice SMF/UPF, grouped into dependency
/// phases: mongo → NRF → the SBI-plane NFs (independent NRF clients, any
/// order within the phase) → AMF → per-slice SMF → per-slice UPF (needs
/// NET_ADMIN + /dev/net/tun for its vxtun device). Every container mounts
/// the rendered config set from `cfg_dir`. Pure — no container engine — so
/// the ordering and per-container capabilities are unit-testable directly.
pub fn core_run_specs(
    plan: &FiveGPlan,
    core_image: &str,
    cfg_dir: &Path,
) -> Vec<Vec<ContainerSpec>> {
    let spec = |name: String, image: &str, cmd: Vec<String>| ContainerSpec {
        name,
        image: image.to_string(),
        cmd,
        binds: vec![config_bind(cfg_dir)],
        ..Default::default()
    };
    let nf = |name: &str| {
        spec(
            name.to_string(),
            core_image,
            vec![
                format!("open5gs-{name}d"),
                "-c".to_string(),
                format!("/etc/vx5g/open5gs/{name}.yaml"),
            ],
        )
    };

    vec![
        vec![spec(
            "mongo".to_string(),
            "docker.io/library/mongo:7",
            vec![
                "mongod".to_string(),
                "--bind_ip".to_string(),
                "127.0.0.1".to_string(),
                "--quiet".to_string(),
            ],
        )],
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
            .map(|s| {
                spec(
                    format!("smf-{}", s.slice_id),
                    core_image,
                    vec![
                        "open5gs-smfd".to_string(),
                        "-c".to_string(),
                        format!("/etc/vx5g/open5gs/smf-{}.yaml", s.slice_id),
                    ],
                )
            })
            .collect(),
        plan.sessions
            .iter()
            .map(|s| ContainerSpec {
                cap_add: vec!["NET_ADMIN".to_string()],
                devices: vec!["/dev/net/tun".to_string()],
                ..spec(
                    format!("upf-{}", s.slice_id),
                    core_image,
                    vec![
                        "open5gs-upfd".to_string(),
                        "-c".to_string(),
                        format!("/etc/vx5g/open5gs/upf-{}.yaml", s.slice_id),
                    ],
                )
            })
            .collect(),
    ]
}

/// The gNB, alone in the RAN pod.
fn gnb_spec(cfg: &FiveGLifecycleConfig, cfg_dir: &Path) -> ContainerSpec {
    ContainerSpec {
        name: "gnb".to_string(),
        image: cfg.ran_image.clone(),
        cmd: vec![
            "nr-gnb".to_string(),
            "-c".to_string(),
            "/etc/vx5g/ueransim/gnb.yaml".to_string(),
        ],
        binds: vec![config_bind(cfg_dir)],
        ..Default::default()
    }
}

/// One UE, alone in its own pod; needs NET_ADMIN + /dev/net/tun for its
/// `uesimtun<i>` device, like a UPF.
fn ue_spec(cfg: &FiveGLifecycleConfig, cfg_dir: &Path, session: &SliceSession) -> ContainerSpec {
    ContainerSpec {
        name: "ue".to_string(),
        image: cfg.ran_image.clone(),
        cmd: vec![
            "nr-ue".to_string(),
            "-c".to_string(),
            format!("/etc/vx5g/ueransim/ue-{}.yaml", session.slice_id),
        ],
        binds: vec![config_bind(cfg_dir)],
        cap_add: vec!["NET_ADMIN".to_string()],
        devices: vec!["/dev/net/tun".to_string()],
        ..Default::default()
    }
}

/// Start each phase's containers in the pod, settling between phases.
async fn start_phases(
    client: &ContainerClient,
    group: &GroupSpec,
    handle: &crate::containers::GroupHandle,
    phases: &[Vec<ContainerSpec>],
) -> Result<(), FiveGLifecycleError> {
    for phase in phases {
        for spec in phase {
            client
                .ensure_member(group, handle, spec)
                .await
                .map_err(containers(format!(
                    "start `{}-{}`",
                    group.name(),
                    spec.name
                )))?;
        }
        tokio::time::sleep(PHASE_SETTLE).await;
    }
    Ok(())
}

/// Join the CN and RAN pods with a veth pair on `TRANSIT_SUBNET` — the
/// mechanism side of the N2/N3 equivalent. Both ends are created on the
/// host, then each is moved into its target pod's netns and addressed
/// (`nsenter`, since podman pods aren't in `ip netns`'s named registry).
///
/// The RAN-side end lands on [`ran_bridge_name`], not directly on the RAN
/// pod's own stack: with each UE now in its own pod/netns (found necessary
/// live, 2026-07-17 — UERANSIM hardcodes routing table `1000` per UE, so
/// UEs sharing one netns collide), gNB needs one address reachable from
/// *both* the CN pod (across this transit veth) and every UE pod (each
/// across its own veth, added later by [`ue_up`]) — a small internal
/// bridge, entirely inside the RAN pod's netns, is what makes one address
/// (`RAN_TRANSIT_ADDR`) reachable from an arbitrary number of separate
/// peers, the same veth-into-bridge primitive used everywhere else in this
/// codebase (`gateway::provision::apply_endpoints`, [`bridge_up`]).
async fn setup_transit_link(
    cfg: &FiveGLifecycleConfig,
    cn_pid: u32,
    ran_pid: u32,
) -> Result<(), FiveGLifecycleError> {
    let (cn_side, ran_side) = transit_veth_names(&cfg.pod_name);
    let bridge = ran_bridge_name(&cfg.ran_pod_name);
    host_ip(&[
        "link", "add", &cn_side, "type", "veth", "peer", "name", &ran_side,
    ])
    .await?;
    host_ip(&["link", "set", &cn_side, "netns", &cn_pid.to_string()]).await?;
    host_ip(&["link", "set", &ran_side, "netns", &ran_pid.to_string()]).await?;

    let cn_addr = format!("{CN_TRANSIT_ADDR}/24");
    nsenter_ip(cn_pid, &["addr", "add", &cn_addr, "dev", &cn_side]).await?;
    nsenter_ip(cn_pid, &["link", "set", &cn_side, "up"]).await?;

    nsenter_ip(ran_pid, &["link", "add", &bridge, "type", "bridge"]).await?;
    nsenter_ip(ran_pid, &["link", "set", &bridge, "up"]).await?;
    nsenter_ip(ran_pid, &["link", "set", &ran_side, "master", &bridge]).await?;
    nsenter_ip(ran_pid, &["link", "set", &ran_side, "up"]).await?;

    let ran_addr = format!("{RAN_TRANSIT_ADDR}/24");
    nsenter_ip(ran_pid, &["addr", "add", &ran_addr, "dev", &bridge]).await?;
    Ok(())
}

/// Configure a slice's UPF tun device — Open5GS creates the raw device but
/// leaves IP configuration to the deployer (standard bare-metal practice;
/// found live, 2026-07-17: it otherwise stays `DOWN`, no route, and nothing
/// else configures it). Assigning `ue_gateway` here is what makes
/// `ue_subnet` a connected route in the CN pod's netns, which is how
/// downlink traffic to a UE's IP finds its way back into the UPF's GTP-U
/// tunnel.
async fn setup_upf_tun(cn_pid: u32, session: &SliceSession) -> Result<(), FiveGLifecycleError> {
    let addr = format!("{}/16", session.ue_gateway);
    nsenter_ip(cn_pid, &["addr", "add", &addr, "dev", &session.tun_dev]).await?;
    nsenter_ip(cn_pid, &["link", "set", &session.tun_dev, "up"]).await?;
    Ok(())
}

/// Bring one UE up in its **own** pod, joined to the RAN pod's gNB-access
/// bridge ([`ran_bridge_name`]) by a dedicated veth. This is the actual fix
/// for the routing-table collision found live 2026-07-17 (the bridging
/// live-proof): UERANSIM hardcodes routing table `1000` for its per-UE
/// default route rather than deriving it from the interface, so two UEs
/// sharing one netns collide the moment the second one registers — a real
/// deployment never hits this (UERANSIM's own intended model is one UE per
/// netns). Giving each UE a fully private pod/netns is the same fix
/// already proven for the CN/RAN split one layer up, applied one layer
/// down. Idempotent: an already-existing UE pod is left untouched.
///
/// The transit link ([`setup_transit_link`], called by [`fiveg_up`]) must
/// already exist — `ran_pid` is the RAN pod's own PID ([`pod_pid`]) and
/// `index` is this session's position in `plan.sessions` (used only for
/// IFNAMSIZ-safe veth naming, [`ue_veth_names`] — pod names use the slice
/// id directly, unconstrained).
pub async fn ue_up(
    client: &ContainerClient,
    cfg: &FiveGLifecycleConfig,
    cfg_dir: &Path,
    ran_pid: u32,
    index: usize,
    session: &SliceSession,
) -> Result<String, FiveGLifecycleError> {
    let pod = ue_pod_name(&cfg.ran_pod_name, &session.slice_id);
    refuse_legacy(client, vec![format!("{pod}-ue")], pod.clone()).await?;
    if pod_exists(client, &pod).await? {
        return Ok(format!("UE pod `{pod}` already up (idempotent)"));
    }
    let group = pod_group(cfg, &pod)?;
    let (handle, _) = client
        .ensure_group(&group)
        .await
        .map_err(containers(format!("bring up UE pod `{pod}`")))?;
    let ue_pid = handle.infra_pid;

    let (ran_side, ue_side) = ue_veth_names(&cfg.ran_pod_name, index);
    let bridge = ran_bridge_name(&cfg.ran_pod_name);
    host_ip(&[
        "link", "add", &ran_side, "type", "veth", "peer", "name", &ue_side,
    ])
    .await?;
    host_ip(&["link", "set", &ran_side, "netns", &ran_pid.to_string()]).await?;
    host_ip(&["link", "set", &ue_side, "netns", &ue_pid.to_string()]).await?;

    nsenter_ip(ran_pid, &["link", "set", &ran_side, "master", &bridge]).await?;
    nsenter_ip(ran_pid, &["link", "set", &ran_side, "up"]).await?;

    let ue_addr = format!("{}/24", session.ue_ran_link_addr);
    nsenter_ip(ue_pid, &["addr", "add", &ue_addr, "dev", &ue_side]).await?;
    nsenter_ip(ue_pid, &["link", "set", &ue_side, "up"]).await?;

    client
        .ensure_member(&group, &handle, &ue_spec(cfg, cfg_dir, session))
        .await
        .map_err(containers(format!("start `{pod}-ue`")))?;

    Ok(format!(
        "UE pod `{pod}` up, joined to gNB via {bridge} at {}",
        session.ue_ran_link_addr
    ))
}

/// Tear one UE's pod down. Idempotent (an absent pod is a no-op) — its
/// veth is destroyed automatically with the pod's netns, same as
/// [`fiveg_down`]'s transit link.
pub async fn ue_down(
    client: &ContainerClient,
    cfg: &FiveGLifecycleConfig,
    session: &SliceSession,
) -> Result<String, FiveGLifecycleError> {
    let pod = ue_pod_name(&cfg.ran_pod_name, &session.slice_id);
    refuse_legacy(client, vec![format!("{pod}-ue")], pod.clone()).await?;
    client
        .group_down(&pod)
        .await
        .map_err(containers(format!("remove UE pod `{pod}`")))?;
    Ok(format!("UE pod `{pod}` down"))
}

/// Timeout waiting for a UE's PDU session to assign `uesimtun0` an address.
/// Registration + PDU Session Establishment normally lands well under this
/// in every live run so far; bounded so a genuinely broken UE fails loudly
/// instead of hanging forever.
const UE_PDU_ADDR_TIMEOUT: Duration = Duration::from_secs(15);

/// Poll a UE's `uesimtun0` for its assigned PDU-session address. Needed by
/// [`ue_probe_up`] because that address is only known once registration
/// completes — asynchronous, no fixed delay is reliable for it (unlike the
/// dependency-phase settling elsewhere in this module, a fixed sleep here
/// risks either failing outright on a slow first run or silently binding
/// too early). A bounded poll is the correct tool for a real readiness
/// condition, not a magic number. `pub` so `traffic::lifecycle` can reuse
/// this exact discovery for its own s-nssai targets rather than
/// reimplementing it a second way.
pub async fn wait_for_ue_pdu_addr(ue_pid: u32) -> Result<String, FiveGLifecycleError> {
    let deadline = std::time::Instant::now() + UE_PDU_ADDR_TIMEOUT;
    loop {
        if let Ok(out) = nsenter_ip(ue_pid, &["-4", "-o", "addr", "show", "uesimtun0"]).await {
            let addr = out
                .split_whitespace()
                .skip_while(|w| *w != "inet")
                .nth(1)
                .and_then(|cidr| cidr.split('/').next());
            if let Some(addr) = addr {
                return Ok(addr.to_string());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(FiveGLifecycleError::Parse {
                cmdline: "ip -4 -o addr show uesimtun0".to_string(),
                detail: format!(
                    "no PDU-session address after {UE_PDU_ADDR_TIMEOUT:?} — UE registration \
                     may not have completed"
                ),
            });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Start a `vxprobe` reflector inside a UE's own pod — shares its netns, so
/// once the PDU session exists it is reachable at the UE's own address
/// (`uesimtun0`), the same way a real ADAS ECU would run the reflector on
/// itself. This is what makes an H2 campaign topologically comparable to
/// H1's own methodology (`vxprobe reflect` running *on* the device under
/// test, probed from the gateway/host side — not pinged from outside as a
/// mechanism spot-check).
///
/// Binds explicitly to the UE's own PDU-session address, not `0.0.0.0` —
/// found live 2026-07-17: a `0.0.0.0`-bound reflector *did* receive every
/// probe (confirmed via `uesimtun0`'s own RX packet counter incrementing by
/// exactly the number sent), but its replies almost never made it back —
/// TX barely moved. Root cause: with an unbound source, the kernel resolves
/// the reply's outbound route before the UE's own policy rule
/// (`from <addr> lookup rt_uesimtun<i>`) can pin it to the tunnel, so
/// replies leaked out via UERANSIM's own unrelated default route (`tap0`,
/// its internal management path) instead. Binding to the specific address
/// forces the correct egress interface regardless — which needs
/// [`wait_for_ue_pdu_addr`] first, since that address doesn't exist until
/// registration completes.
///
/// Idempotent: an existing reflector container is left running. Torn down
/// automatically by [`ue_down`] (destroys the whole pod, not addressed
/// separately here).
pub async fn ue_probe_up(
    client: &ContainerClient,
    cfg: &FiveGLifecycleConfig,
    cfg_dir: &Path,
    session: &SliceSession,
    port: u16,
) -> Result<String, FiveGLifecycleError> {
    let pod = ue_pod_name(&cfg.ran_pod_name, &session.slice_id);
    let status = client
        .group_status(&pod)
        .await
        .map_err(containers(format!("observe UE pod `{pod}`")))?;
    let probe_name = format!("{pod}-probe");
    if status.member(&probe_name).is_some() {
        return Ok(format!(
            "vxprobe reflector already up in UE pod `{pod}` (idempotent)"
        ));
    }
    let handle = client.handle(&pod).await.map_err(containers(format!(
        "find UE pod `{pod}`'s network namespace"
    )))?;
    let ue_addr = wait_for_ue_pdu_addr(handle.infra_pid).await?;
    let probe = ContainerSpec {
        name: "probe".to_string(),
        image: cfg.ran_image.clone(),
        cmd: vec![
            "vxprobe".to_string(),
            "reflect".to_string(),
            "--listen".to_string(),
            format!("{ue_addr}:{port}"),
        ],
        binds: vec![config_bind(cfg_dir)],
        ..Default::default()
    };
    client
        .ensure_member(&pod_group(cfg, &pod)?, &handle, &probe)
        .await
        .map_err(containers(format!("start `{probe_name}`")))?;
    Ok(format!(
        "vxprobe reflector up in UE pod `{pod}` on {ue_addr}:{port}"
    ))
}

/// Address for the CN pod's side of [`bridge_up`]'s attachment — distinct
/// from the CN↔RAN transit subnet, since this is a different link (CN pod
/// ↔ bridge, not CN pod ↔ RAN pod).
pub const BRIDGE_ATTACH_SUBNET: &str = "10.98.0.0/24";
pub const BRIDGE_ATTACH_CN_ADDR: &str = "10.98.0.1";
/// The *host's own* address on this subnet — found necessary live
/// 2026-07-17 wiring `TrafficService`: a host-generated `vxprobe send`
/// targeting a UE's PDU-session address got 100% loss even though the CN
/// pod's own port had correct VLAN membership (`apply_fiveg_uplink`),
/// because *nothing* gave the host's own network stack a route into
/// `BRIDGE_ATTACH_SUBNET` at all — `gateway::provision::apply_fiveg_uplink`
/// now also creates a `<bridge>.<FIVEG_UPLINK_VLAN>` subinterface addressed
/// here, the s-nssai equivalent of the vlan path's `<uplink_peer>.<vlan>`
/// convention.
pub const BRIDGE_ATTACH_HOST_ADDR: &str = "10.98.0.2";

fn bridge_veth_names(pod_name: &str) -> (String, String) {
    let base: String = pod_name.chars().take(10).collect();
    (format!("{base}-br0"), format!("{base}-brp0"))
}

/// Host-side device [`bridge_up`] plugs into `bridge` — the port a caller
/// needs to name when deciding VLAN membership on a `vlan_filtering 1`
/// bridge (`vxbr0`), since `bridge_up` itself deliberately does not decide
/// that (see its doc comment).
pub fn bridge_host_veth(pod_name: &str) -> String {
    bridge_veth_names(pod_name).0
}

/// Reserved VLAN for the 5G core's uplink port on `vxgateway`'s bridge.
/// S-NSSAI slices carry no `vlan:` in the manifest — slice differentiation
/// happens in the RAN/PDU-session layer, one level below where 802.1Q
/// operates — so there is no per-slice VLAN to reuse for the CN pod's
/// attachment. The CN pod is one network function with one attachment
/// point: by the time traffic reaches a UPF's uplink, S-NSSAI
/// differentiation has already happened. Fixed and out of the range any
/// existing experiment manifest declares for a vlan-mode slice (checked:
/// exp-001/002/002b/003 all use IDs well under 1000).
pub const FIVEG_UPLINK_VLAN: u16 = 4000;

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
pub async fn bridge_up(
    cfg: &FiveGLifecycleConfig,
    bridge: &str,
    cn_pid: u32,
) -> Result<(), FiveGLifecycleError> {
    // Converges rather than creates, so a second `experiment up` over a
    // running core is a no-op (it failed with `File exists` before):
    // `fiveg_up` keeps an existing CN pod, and with it this veth pair,
    // whose two ends live and die together.
    let (host_side, pod_side) = bridge_veth_names(&cfg.pod_name);
    if !host_link_exists(&host_side).await? {
        host_ip(&[
            "link", "add", &host_side, "type", "veth", "peer", "name", &pod_side,
        ])
        .await?;
    }
    // Still in the host namespace only if an earlier run stopped between
    // creating the pair and moving this end.
    if host_link_exists(&pod_side).await? {
        host_ip(&["link", "set", &pod_side, "netns", &cn_pid.to_string()]).await?;
    }

    host_ip(&["link", "set", &host_side, "master", bridge]).await?;
    host_ip(&["link", "set", &host_side, "up"]).await?;

    let cn_addr = format!("{BRIDGE_ATTACH_CN_ADDR}/24");
    tolerate_exists(nsenter_ip(cn_pid, &["addr", "add", &cn_addr, "dev", &pod_side]).await)?;
    nsenter_ip(cn_pid, &["link", "set", &pod_side, "up"]).await?;

    // The CN pod's netns now has two interfaces a packet might need to
    // cross between (this bridge-side veth and a UPF's vxtun<i>) — off by
    // default in a fresh netns, so forwarding between them needs enabling
    // explicitly, same as any Linux router.
    nsenter_run(cn_pid, "sysctl", &["-w", "net.ipv4.ip_forward=1"]).await?;
    Ok(())
}

/// Whether `name` exists in the host's own network namespace. Reading
/// links needs no privilege, so this runs `ip` directly, not through sudo.
async fn host_link_exists(name: &str) -> Result<bool, FiveGLifecycleError> {
    Command::new("ip")
        .args(["link", "show", "dev", name])
        .output()
        .await
        .map(|out| out.status.success())
        .map_err(|source| FiveGLifecycleError::Spawn {
            program: "ip".to_string(),
            source,
        })
}

/// An `ip addr add` that found the address already assigned has converged.
/// Older iproute2 says "File exists", 6.19 "Address already assigned";
/// `gateway::netops::Shell::run_tolerate_exists` matches the same two.
fn tolerate_exists(result: Result<String, FiveGLifecycleError>) -> Result<(), FiveGLifecycleError> {
    match result {
        Err(FiveGLifecycleError::Command { stderr, .. })
            if stderr.contains("File exists") || stderr.contains("already assigned") =>
        {
            Ok(())
        }
        other => other.map(|_| ()),
    }
}

/// Remove the bridge attachment. Idempotent: deleting the host-side veth
/// also destroys its pod-side peer, wherever it currently lives; a missing
/// interface is tolerated (`ip link del` failing with "Cannot find device"
/// means it's already gone, not an error worth surfacing).
pub async fn bridge_down(cfg: &FiveGLifecycleConfig) -> Result<(), FiveGLifecycleError> {
    let (host_side, _) = bridge_veth_names(&cfg.pod_name);
    match host_ip(&["link", "del", &host_side]).await {
        Ok(_) => Ok(()),
        Err(FiveGLifecycleError::Command { stderr, .. })
            if stderr.contains("Cannot find device") || stderr.contains("does not exist") =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Containers the podman-CLI code put in the CN and RAN pods, and the pods
/// to remove (UE pods included, found by name) — see [`refuse_legacy`].
fn legacy_core_check(cfg: &FiveGLifecycleConfig) -> (Vec<String>, String) {
    let (cn, ran) = (&cfg.pod_name, &cfg.ran_pod_name);
    (
        vec![
            format!("{cn}-mongo"),
            format!("{cn}-nrf"),
            format!("{ran}-gnb"),
        ],
        format!("{cn} {ran} $(podman pod ps -q --filter name={ran}-ue-)"),
    )
}

/// Bring the 5G core up as a running pod. Idempotent: an already-existing
/// pod is left untouched (matches `services.rs::svc_up`'s convention).
pub async fn fiveg_up(
    client: &ContainerClient,
    plan: &FiveGPlan,
    runtime_dir: &Path,
    cfg: &FiveGLifecycleConfig,
) -> Result<String, FiveGLifecycleError> {
    let (legacy, pods) = legacy_core_check(cfg);
    refuse_legacy(client, legacy, pods).await?;
    if pod_exists(client, &cfg.pod_name).await? {
        return Ok(format!("pod `{}` already up (idempotent)", cfg.pod_name));
    }

    let cfg_dir = runtime_dir.join("fiveg-config");
    write_all(plan, &cfg_dir).map_err(|source| FiveGLifecycleError::Render {
        dir: cfg_dir.display().to_string(),
        source,
    })?;

    // CN pod: the 8-NF core + per-slice SMF/UPF, in dependency order.
    let cn = pod_group(cfg, &cfg.pod_name)?;
    let (cn_handle, _) = client
        .ensure_group(&cn)
        .await
        .map_err(containers(format!("bring up CN pod `{}`", cfg.pod_name)))?;

    let phases = core_run_specs(plan, &cfg.core_image, &cfg_dir);
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
    start_phases(client, &cn, &cn_handle, pre_transit_phases).await?;

    // RAN pod: gNB only — individual UEs are started per test run against
    // it (the vxprobe/vxtraffic pattern: provisioning here, traffic
    // driven separately).
    let ran = pod_group(cfg, &cfg.ran_pod_name)?;
    let (ran_handle, _) = client.ensure_group(&ran).await.map_err(containers(format!(
        "bring up RAN pod `{}`",
        cfg.ran_pod_name
    )))?;

    // Join the two pods (assigns CN_TRANSIT_ADDR — what AMF's NGAP server
    // binds to), then give each slice's UPF-to-be its own GTP-U address on
    // the CN side of the link too — everything the phases below need must
    // exist before they start.
    let (cn_pid, ran_pid) = (cn_handle.infra_pid, ran_handle.infra_pid);
    setup_transit_link(cfg, cn_pid, ran_pid).await?;
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
        )
        .await?;
    }

    // Now amf, smf, and upf — their transit-subnet bind addresses all
    // exist, so every daemon starts and stays up this time.
    start_phases(client, &cn, &cn_handle, post_transit_phases).await?;
    for session in &plan.sessions {
        setup_upf_tun(cn_pid, session).await?;
    }

    start_phases(client, &ran, &ran_handle, &[vec![gnb_spec(cfg, &cfg_dir)]]).await?;

    // One-shots in the CN pod's namespace, where mongo listens on loopback
    // (`podman run --rm --pod` before V2).
    for (i, cmd) in subscriber_insert_commands(plan).into_iter().enumerate() {
        let insert = ContainerSpec {
            name: format!("subscriber-{i}"),
            image: cfg.core_image.clone(),
            cmd,
            ..Default::default()
        };
        client
            .run_to_completion(&cn, &cn_handle, &insert)
            .await
            .map_err(containers(format!(
                "provision subscriber for slice `{}`",
                plan.sessions[i].slice_id
            )))?;
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

/// Tear both pods down. Idempotent: an already-absent pod is a no-op. The
/// transit veth is destroyed automatically when either end's netns is
/// destroyed — no explicit cleanup needed.
pub async fn fiveg_down(
    client: &ContainerClient,
    cfg: &FiveGLifecycleConfig,
) -> Result<String, FiveGLifecycleError> {
    let (legacy, pods) = legacy_core_check(cfg);
    refuse_legacy(client, legacy, pods).await?;
    for pod in [&cfg.ran_pod_name, &cfg.pod_name] {
        client
            .group_down(pod)
            .await
            .map_err(containers(format!("remove pod `{pod}`")))?;
    }
    Ok(format!(
        "pods `{}` and `{}` down",
        cfg.pod_name, cfg.ran_pod_name
    ))
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

async fn pod_containers(
    client: &ContainerClient,
    pod_name: &str,
) -> Result<(bool, Vec<ContainerStatus>), FiveGLifecycleError> {
    let status = client
        .group_status(pod_name)
        .await
        .map_err(containers(format!("observe pod `{pod_name}`")))?;
    let containers = status
        .members
        .iter()
        .map(|c| ContainerStatus {
            name: c.name.clone(),
            state: c.state.clone(),
        })
        .collect();
    Ok((status.exists(), containers))
}

/// Observe both pods' actual state — the reconciler's read side, mirroring
/// `gateway::provision::observe`.
pub async fn fiveg_status(
    client: &ContainerClient,
    cfg: &FiveGLifecycleConfig,
) -> Result<FiveGStatus, FiveGLifecycleError> {
    let (up, containers) = pod_containers(client, &cfg.pod_name).await?;
    let (ran_up, ran_containers) = pod_containers(client, &cfg.ran_pod_name).await?;
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

    fn plan_two_slices() -> FiveGPlan {
        let yaml = r#"
kind: experiment
id: t
vehicles: [veh]
network:
  slices:
    - { id: adas, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }
    - { id: infotainment, mode: s-nssai, s_nssai: { sst: 2 } }
---
kind: vehicle
id: veh
devices: {}
"#;
        let m = crate::model::load_yaml(yaml).expect("valid test workspace");
        FiveGPlan::from_model(&m).expect("plans")
    }

    #[test]
    fn phases_are_ordered_mongo_nrf_sbi_amf_smf_upf() {
        let phases = core_run_specs(
            &plan_two_slices(),
            "vx5g-core",
            Path::new("/run/vx/fiveg-config"),
        );
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
        let phases = core_run_specs(
            &plan_two_slices(),
            "vx5g-core",
            Path::new("/run/vx/fiveg-config"),
        );
        let upf_phase = phases.last().unwrap();
        for spec in upf_phase {
            assert_eq!(spec.cap_add, ["NET_ADMIN"]);
            assert_eq!(spec.devices, ["/dev/net/tun"]);
        }
        // mongo, nrf, and the SBI-plane NFs need neither.
        for phase in &phases[..4] {
            for spec in phase {
                assert!(
                    spec.cap_add.is_empty() && spec.devices.is_empty(),
                    "{} should need no capability or device",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn every_core_container_mounts_the_rendered_configs_read_only() {
        let cfg_dir = Path::new("/run/vx/fiveg-config");
        for spec in core_run_specs(&plan_two_slices(), "vx5g-core", cfg_dir)
            .iter()
            .flatten()
        {
            // `-v <cfg>:/etc/vx5g:ro,Z`, as the podman-CLI path passed it.
            assert_eq!(spec.binds, [config_bind(cfg_dir)], "{}", spec.name);
            assert_eq!(
                spec.binds[0].to_bind_string(),
                "/run/vx/fiveg-config:/etc/vx5g:ro,Z"
            );
        }
    }

    #[test]
    fn gnb_and_ue_specs_match_what_the_cli_ran() {
        let cfg = FiveGLifecycleConfig::default();
        let cfg_dir = Path::new("/run/vx/fiveg-config");
        let gnb = gnb_spec(&cfg, cfg_dir);
        assert_eq!(gnb.image, "vx5g-ran");
        assert_eq!(gnb.cmd, ["nr-gnb", "-c", "/etc/vx5g/ueransim/gnb.yaml"]);
        assert!(gnb.cap_add.is_empty() && gnb.devices.is_empty());

        let session = &plan_two_slices().sessions[0];
        let ue = ue_spec(&cfg, cfg_dir, session);
        assert_eq!(ue.name, "ue");
        assert_eq!(ue.cmd, ["nr-ue", "-c", "/etc/vx5g/ueransim/ue-adas.yaml"]);
        assert_eq!(ue.cap_add, ["NET_ADMIN"]);
        assert_eq!(ue.devices, ["/dev/net/tun"]);
    }

    #[test]
    fn legacy_check_names_exact_member_containers_and_every_pod() {
        let (names, pods) = legacy_core_check(&FiveGLifecycleConfig::default());
        assert_eq!(names, ["vx5g-mongo", "vx5g-nrf", "vx5g-ran-gnb"]);
        assert!(pods.starts_with("vx5g vx5g-ran "), "{pods}");
        assert!(pods.contains("name=vx5g-ran-ue-"), "UE pods too: {pods}");
    }

    #[test]
    fn nf_configs_point_at_their_own_rendered_file() {
        let phases = core_run_specs(
            &plan_two_slices(),
            "vx5g-core",
            Path::new("/run/vx/fiveg-config"),
        );
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
        assert_eq!(cfg.infra_image, PAUSE_IMAGE);
        assert!(
            PAUSE_IMAGE.contains("@sha256:"),
            "the infra image is digest-pinned"
        );
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
    fn an_address_already_assigned_counts_as_converged() {
        let exists = || FiveGLifecycleError::Command {
            cmdline: "sudo nsenter -t 1 -n -- ip addr add 10.98.0.1/24 dev vx5g-brp0".into(),
            stderr: "RTNETLINK answers: File exists".into(),
        };
        assert!(tolerate_exists(Err(exists())).is_ok());
        let assigned = FiveGLifecycleError::Command {
            cmdline: "sudo nsenter -t 1 -n -- ip addr add 10.98.0.1/24 dev vx5g-brp0".into(),
            stderr: "Error: ipv4: Address already assigned.".into(),
        };
        assert!(
            tolerate_exists(Err(assigned)).is_ok(),
            "iproute2 6.19 wording"
        );
        assert!(tolerate_exists(Ok(String::new())).is_ok());
        let other = FiveGLifecycleError::Command {
            cmdline: "ip".into(),
            stderr: "Cannot find device \"vx5g-brp0\"".into(),
        };
        assert!(tolerate_exists(Err(other)).is_err());
    }

    #[test]
    fn bridge_attach_addressing_is_distinct_from_transit_subnet() {
        assert_ne!(BRIDGE_ATTACH_SUBNET, super::super::plan::TRANSIT_SUBNET);
        assert_eq!(BRIDGE_ATTACH_CN_ADDR, "10.98.0.1");
    }

    #[test]
    fn ran_bridge_name_fits_ifnamsiz_and_is_derived_from_ran_pod_name() {
        let br = ran_bridge_name("vx5g-ran");
        assert_eq!(br, "vx5g-ran-br");
        assert!(br.len() <= 15, "{br} exceeds IFNAMSIZ");

        let br = ran_bridge_name("a-very-long-custom-ran-pod-name");
        assert!(br.len() <= 15, "{br} exceeds IFNAMSIZ");
    }

    #[test]
    fn ue_veth_names_fit_ifnamsiz_and_are_distinct_per_index() {
        let (ran0, ue0) = ue_veth_names("vx5g-ran", 0);
        let (ran1, ue1) = ue_veth_names("vx5g-ran", 1);
        assert_ne!(ran0, ran1, "different UEs must get different veth names");
        assert_ne!(ue0, ue1);
        for name in [&ran0, &ue0, &ran1, &ue1] {
            assert!(name.len() <= 15, "{name} exceeds IFNAMSIZ");
        }

        // Max slice index (MAX_SNSSAI_SLICES - 1 = 15) must still fit.
        let (ran15, ue15) = ue_veth_names("a-very-long-custom-ran-pod-name", 15);
        assert!(ran15.len() <= 15, "{ran15} exceeds IFNAMSIZ");
        assert!(ue15.len() <= 15, "{ue15} exceeds IFNAMSIZ");
    }

    #[test]
    fn ue_pod_name_is_stable_and_slice_id_keyed() {
        assert_eq!(ue_pod_name("vx5g-ran", "adas"), "vx5g-ran-ue-adas");
        assert_ne!(
            ue_pod_name("vx5g-ran", "adas"),
            ue_pod_name("vx5g-ran", "infotainment")
        );
    }
}
