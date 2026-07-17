//! Podman-backed lifecycle for the containerized 5G core (track C, hybrid
//! policy §15). Same shape as `services.rs`'s process backend: idempotent
//! up/down, state observed rather than assumed. Where `services.rs` tracks
//! one pidfile per process, this tracks one **pod** (podman's shared-netns
//! container group — `containers/fiveg/pod-up.sh` is the shell reference
//! this module replaces with a tested, typed Rust path).
//!
//! Scope note: this module is the *mechanism* — bring a `FiveGPlan` up as a
//! running pod, observe it, tear it down. It is not yet wired to any vxAPI
//! RPC or manifest lifecycle verb; that wiring (GatewayService recognizing
//! `mode: s-nssai` slices) is tracked separately (`vxedge/TASKS.md` §4) since
//! it is a proto/RPC design decision, not a mechanism one.
//!
//! Two-layer split for testability (rust-patterns: pure logic vs. I/O):
//! [`core_run_specs`] (here) and [`super::subscriber::subscriber_insert_commands`]
//! are pure — they describe *what* to run, unit-tested without touching
//! podman. [`fiveg_up`] and friends are the thin, untested-except-by-the-
//! ignored-integration-test layer that actually execs `podman`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::plan::FiveGPlan;
use super::render::write_all;
use super::subscriber::subscriber_insert_commands;

#[derive(Debug, thiserror::Error)]
pub enum FiveGLifecycleError {
    #[error(
        "cannot execute `podman`: {source} (is podman installed? see containers/fiveg/README.md)"
    )]
    Spawn { source: std::io::Error },
    #[error("`podman {args}` failed: {stderr}")]
    Command { args: String, stderr: String },
    #[error("cannot write rendered configs to {dir}: {source}")]
    Render { dir: String, source: std::io::Error },
    #[error("cannot parse `podman ps` output: {0}")]
    Parse(String),
}

/// Which pod + image to drive. Defaults match `containers/fiveg/`.
#[derive(Debug, Clone)]
pub struct FiveGLifecycleConfig {
    pub pod_name: String,
    pub core_image: String,
}

impl Default for FiveGLifecycleConfig {
    fn default() -> Self {
        Self {
            pod_name: "vx5g".to_string(),
            core_image: "vx5g-core".to_string(),
        }
    }
}

/// Settle time between dependency phases. A fixed sleep, not a health poll —
/// matches `pod-up.sh`'s proven-live timings; polling a readiness signal
/// (e.g. NRF's SBI port) is a tracked hardening follow-up, not a correctness
/// gap in what's shipped today.
const PHASE_SETTLE: Duration = Duration::from_secs(2);

fn podman(args: &[String]) -> Result<String, FiveGLifecycleError> {
    let out = Command::new("podman")
        .args(args)
        .output()
        .map_err(|source| FiveGLifecycleError::Spawn { source })?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(FiveGLifecycleError::Command {
        args: args.join(" "),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// True if the pod is up (any state) — the idempotency check for `up`, and
/// the fast path for `status`/`down` on an already-absent pod.
pub fn pod_exists(pod_name: &str) -> Result<bool, FiveGLifecycleError> {
    match Command::new("podman")
        .args(["pod", "exists", pod_name])
        .status()
    {
        Ok(status) => Ok(status.success()),
        Err(source) => Err(FiveGLifecycleError::Spawn { source }),
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

    podman(&[
        "pod".to_string(),
        "create".to_string(),
        "--name".to_string(),
        cfg.pod_name.clone(),
    ])?;

    let phases = core_run_specs(plan, &cfg.core_image);
    let phase_count = phases.len();
    for phase in phases {
        for spec in &phase {
            run_detached(&cfg.pod_name, &cfg_dir, spec)?;
        }
        std::thread::sleep(PHASE_SETTLE);
    }

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
        "pod `{}` up: {} dependency phases, {} subscribers provisioned",
        cfg.pod_name,
        phase_count,
        plan.sessions.len()
    ))
}

/// Tear the pod down. Idempotent: an already-absent pod is a no-op
/// (`podman pod rm --ignore`).
pub fn fiveg_down(cfg: &FiveGLifecycleConfig) -> Result<String, FiveGLifecycleError> {
    podman(&[
        "pod".to_string(),
        "rm".to_string(),
        "-f".to_string(),
        "--ignore".to_string(),
        cfg.pod_name.clone(),
    ])?;
    Ok(format!("pod `{}` down", cfg.pod_name))
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
}

impl FiveGStatus {
    /// All NF containers (excludes the pod's infra/pause container) report
    /// `running`.
    pub fn all_running(&self) -> bool {
        self.up
            && !self.containers.is_empty()
            && self.containers.iter().all(|c| c.state == "running")
    }
}

/// Observe the pod's actual state — the reconciler's read side, mirroring
/// `gateway::provision::observe`.
pub fn fiveg_status(cfg: &FiveGLifecycleConfig) -> Result<FiveGStatus, FiveGLifecycleError> {
    if !pod_exists(&cfg.pod_name)? {
        return Ok(FiveGStatus {
            pod_name: cfg.pod_name.clone(),
            up: false,
            containers: vec![],
        });
    }
    let out = podman(&[
        "ps".to_string(),
        "--pod".to_string(),
        "--filter".to_string(),
        format!("pod={}", cfg.pod_name),
        "--format".to_string(),
        "json".to_string(),
    ])?;
    let entries: Vec<PsEntry> =
        serde_json::from_str(&out).map_err(|e| FiveGLifecycleError::Parse(e.to_string()))?;
    let containers = entries
        .into_iter()
        .filter(|e| !e.is_infra)
        .map(|e| ContainerStatus {
            name: e.names.into_iter().next().unwrap_or_default(),
            state: e.state,
        })
        .collect();
    Ok(FiveGStatus {
        pod_name: cfg.pod_name.clone(),
        up: true,
        containers,
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
    }

    #[test]
    fn all_running_is_false_when_pod_down() {
        let s = FiveGStatus {
            pod_name: "vx5g".into(),
            up: false,
            containers: vec![],
        };
        assert!(!s.all_running());
    }

    #[test]
    fn all_running_requires_every_container_running() {
        let s = FiveGStatus {
            pod_name: "vx5g".into(),
            up: true,
            containers: vec![
                ContainerStatus {
                    name: "vx5g-nrf".into(),
                    state: "running".into(),
                },
                ContainerStatus {
                    name: "vx5g-amf".into(),
                    state: "exited".into(),
                },
            ],
        };
        assert!(!s.all_running());
    }
}
