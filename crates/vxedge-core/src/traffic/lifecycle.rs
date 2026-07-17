//! Side-effecting stage: provision the endpoints a [`TrafficPlan`] resolved,
//! then run real `vxprobe`/`vxtraffic` processes against them — the actual
//! replacement for what `vxperiments/scripts/run-exp-001.sh` hand-assembles
//! today with raw `ip addr`/`ip link add ... type vlan` and hardcoded
//! addresses.
//!
//! `mode: vlan` endpoints are provisioned here directly via
//! [`gateway::netops::Shell`] (idempotent, JSON-observed reads,
//! argument-vector execs — `network-provisioning-patterns`). `mode: s-nssai`
//! endpoints reuse `fiveg::lifecycle` outright rather than reimplementing
//! UE discovery or reflector startup a second way: [`fiveg::ue_probe_up`]
//! for a probe's reflector (it already fixes the UE-netns `0.0.0.0`-bind
//! bug — reimplementing a plain host-side reflector for an s-nssai target
//! here would silently reintroduce it), [`fiveg::wait_for_ue_pdu_addr`] for
//! a sender's target address.
//!
//! Spawned processes (`vxprobe send`, `vxtraffic`) run as host-local
//! children, pidfile-tracked per profile+role — mirrors `services.rs`'s own
//! process backend exactly, including the "reap on a background thread so a
//! killed child doesn't become a zombie forever" lesson recorded there.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::plan::{ProfileEndpoint, ResolvedProfile};
use crate::fiveg::{self, FiveGLifecycleConfig, FiveGPlan};
use crate::gateway::{NetOpsError, Shell};

#[derive(Debug, thiserror::Error)]
pub enum TrafficLifecycleError {
    #[error("profile `{0}`: provisioning its endpoint failed: {1}")]
    Provision(String, #[source] NetOpsError),
    #[error("profile `{0}`: s-nssai UE reflector/discovery failed: {1}")]
    UeDiscovery(String, #[source] fiveg::FiveGLifecycleError),
    #[error(
        "profile `{0}` targets the CN pod's bridge-attach address for a probe (`pps` set) — \
         that address is only a valid target for a one-directional load profile \
         (`bandwidth_mbps`), since nothing reflects there"
    )]
    ProbeTargetNotReflectable(String),
    #[error(
        "profile `{0}` targets slice `{1}`, but no `FiveGPlan` (or no session for that slice) \
         was supplied — an s-nssai target needs the plan its `ue_probe_up` reflector runs against"
    )]
    MissingFiveGSession(String, String),
    #[error("profile `{profile}` ({role}): cannot spawn `{bin}`: {source}")]
    Spawn {
        profile: String,
        role: &'static str,
        bin: String,
        source: std::io::Error,
    },
    #[error("profile `{0}`: cannot write pidfile/log: {1}")]
    Pidfile(String, std::io::Error),
}

fn pidfile(runtime_dir: &Path, profile_id: &str, role: &str) -> PathBuf {
    runtime_dir.join(format!("traffic-{profile_id}-{role}.pid"))
}

fn read_pid(runtime_dir: &Path, profile_id: &str, role: &str) -> Option<i32> {
    std::fs::read_to_string(pidfile(runtime_dir, profile_id, role))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn is_up(runtime_dir: &Path, profile_id: &str, role: &str) -> bool {
    read_pid(runtime_dir, profile_id, role)
        .map(alive)
        .unwrap_or(false)
}

/// Idempotently provision a `mode: vlan` endpoint's interface + address —
/// near-side (`VlanNodePeer`) just needs (re-)addressing, since
/// `gateway::provision::apply_endpoints` already created the interface
/// itself; far-side (`VlanUplinkFarSide`) needs its VLAN subinterface
/// created too. No-op for `mode: s-nssai` endpoints, which are podman-pod
/// managed, not host-interface managed.
fn ensure_endpoint(
    sh: &Shell,
    profile_id: &str,
    ep: &ProfileEndpoint,
) -> Result<(), TrafficLifecycleError> {
    let provision = |iface: &str, addr: &str, create_vlan: Option<(&str, u16)>| {
        if let Some((parent, vlan)) = create_vlan
            && !sh.link_exists(iface)
        {
            sh.run(
                "ip",
                &[
                    "link",
                    "add",
                    "link",
                    parent,
                    "name",
                    iface,
                    "type",
                    "vlan",
                    "id",
                    &vlan.to_string(),
                ],
            )?;
        }
        sh.run_tolerate_exists("ip", &["addr", "add", &format!("{addr}/24"), "dev", iface])?;
        sh.run("ip", &["link", "set", iface, "up"])?;
        Ok::<(), NetOpsError>(())
    };

    match ep {
        ProfileEndpoint::VlanNodePeer {
            peer_iface, addr, ..
        } => provision(peer_iface, addr, None)
            .map_err(|e| TrafficLifecycleError::Provision(profile_id.to_string(), e)),
        ProfileEndpoint::VlanUplinkFarSide {
            uplink_peer,
            vlan,
            addr,
        } => {
            let iface = format!("{uplink_peer}.{vlan}");
            provision(&iface, addr, Some((uplink_peer, *vlan)))
                .map_err(|e| TrafficLifecycleError::Provision(profile_id.to_string(), e))
        }
        ProfileEndpoint::SnssaiUe { .. } | ProfileEndpoint::SnssaiFarSide => Ok(()),
    }
}

/// Resolve an endpoint's real transport address for spawning `vxprobe`/
/// `vxtraffic` against — `mode: vlan` addresses are already known from the
/// plan (deterministic); `mode: s-nssai` addresses need live discovery
/// (UE) or a fixed constant (CN pod bridge-attach).
fn transport_addr(
    fiveg_cfg: &FiveGLifecycleConfig,
    profile_id: &str,
    ep: &ProfileEndpoint,
) -> Result<String, TrafficLifecycleError> {
    match ep {
        ProfileEndpoint::VlanNodePeer { addr, .. }
        | ProfileEndpoint::VlanUplinkFarSide { addr, .. } => Ok(addr.clone()),
        ProfileEndpoint::SnssaiUe { slice_id } => {
            let ue_pod = fiveg::ue_pod_name(&fiveg_cfg.ran_pod_name, slice_id);
            let ue_pid = fiveg::pod_pid(&ue_pod)
                .map_err(|e| TrafficLifecycleError::UeDiscovery(profile_id.to_string(), e))?;
            fiveg::wait_for_ue_pdu_addr(ue_pid)
                .map_err(|e| TrafficLifecycleError::UeDiscovery(profile_id.to_string(), e))
        }
        ProfileEndpoint::SnssaiFarSide => Ok(fiveg::BRIDGE_ATTACH_CN_ADDR.to_string()),
    }
}

fn spawn_tracked(
    runtime_dir: &Path,
    profile_id: &str,
    role: &'static str,
    bin: &str,
    args: &[String],
) -> Result<u32, TrafficLifecycleError> {
    let log_path = runtime_dir.join(format!("traffic-{profile_id}-{role}.log"));
    let log = std::fs::File::create(&log_path)
        .map_err(|e| TrafficLifecycleError::Pidfile(profile_id.to_string(), e))?;
    let log_err = log
        .try_clone()
        .map_err(|e| TrafficLifecycleError::Pidfile(profile_id.to_string(), e))?;
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|source| TrafficLifecycleError::Spawn {
            profile: profile_id.to_string(),
            role,
            bin: bin.to_string(),
            source,
        })?;
    let pid = child.id();
    std::fs::write(pidfile(runtime_dir, profile_id, role), pid.to_string())
        .map_err(|e| TrafficLifecycleError::Pidfile(profile_id.to_string(), e))?;
    // See services.rs::svc_up's own comment: reap on a background thread so
    // a later-killed child doesn't linger as a zombie that `alive()` keeps
    // reporting as running.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

/// Ensure a reflector is listening at `profile.target` — a plain host-side
/// `vxprobe reflect` for `mode: vlan` targets, or `fiveg::ue_probe_up` for
/// an s-nssai UE target (never a plain reflector there: that would
/// reintroduce the `0.0.0.0`-bind bug `ue_probe_up` already fixes).
/// `five_g_plan`/session validity is checked *before* any real discovery is
/// attempted (never call `fiveg::ue_probe_up` — which does its own live
/// `wait_for_ue_pdu_addr` — until we already know we have a session to run
/// it against), so a misconfigured profile fails fast and cheaply instead
/// of timing out against a UE pod that may not even exist yet. Returns the
/// resolved target address: for `SnssaiUe`, `ue_probe_up` already
/// discovered it internally, so this re-resolves it via the same
/// [`transport_addr`] path rather than threading a second return value out
/// of `fiveg`'s own function — cheap, since the address already exists by
/// this point and the discovery poll returns on its first check.
fn ensure_reflector(
    runtime_dir: &Path,
    fiveg_cfg: &FiveGLifecycleConfig,
    five_g_plan: Option<&FiveGPlan>,
    vxprobe_bin: &str,
    profile: &ResolvedProfile,
    port: u16,
) -> Result<String, TrafficLifecycleError> {
    match &profile.target {
        ProfileEndpoint::SnssaiFarSide => Err(TrafficLifecycleError::ProbeTargetNotReflectable(
            profile.id.clone(),
        )),
        ProfileEndpoint::SnssaiUe { slice_id } => {
            let plan = five_g_plan.ok_or_else(|| {
                TrafficLifecycleError::MissingFiveGSession(profile.id.clone(), slice_id.clone())
            })?;
            let session = plan
                .sessions
                .iter()
                .find(|s| &s.slice_id == slice_id)
                .ok_or_else(|| {
                    TrafficLifecycleError::MissingFiveGSession(profile.id.clone(), slice_id.clone())
                })?;
            let cfg_dir = runtime_dir.join("fiveg-config");
            fiveg::ue_probe_up(fiveg_cfg, &cfg_dir, session, port)
                .map_err(|e| TrafficLifecycleError::UeDiscovery(profile.id.clone(), e))?;
            transport_addr(fiveg_cfg, &profile.id, &profile.target)
        }
        ProfileEndpoint::VlanNodePeer { .. } | ProfileEndpoint::VlanUplinkFarSide { .. } => {
            let target_addr = transport_addr(fiveg_cfg, &profile.id, &profile.target)?;
            if !is_up(runtime_dir, &profile.id, "reflect") {
                spawn_tracked(
                    runtime_dir,
                    &profile.id,
                    "reflect",
                    vxprobe_bin,
                    &[
                        "reflect".to_string(),
                        "--listen".to_string(),
                        format!("{target_addr}:{port}"),
                    ],
                )?;
            }
            Ok(target_addr)
        }
    }
}

/// Bring one resolved profile up: provision both endpoints, start a
/// reflector if this is a round-trip probe (`pps` set), then spawn the
/// actual `vxprobe send`/`vxtraffic` sender. Idempotent per role: an
/// already-running process for a given profile+role is left alone.
#[allow(clippy::too_many_arguments)]
pub fn profile_up(
    sh: &Shell,
    fiveg_cfg: &FiveGLifecycleConfig,
    five_g_plan: Option<&FiveGPlan>,
    runtime_dir: &Path,
    vxprobe_bin: &str,
    vxtraffic_bin: &str,
    profile: &ResolvedProfile,
    port: u16,
) -> Result<String, TrafficLifecycleError> {
    ensure_endpoint(sh, &profile.id, &profile.source)?;
    ensure_endpoint(sh, &profile.id, &profile.target)?;

    if let Some(pps) = profile.pps {
        // Validates the reflector side (fails fast on a misconfigured
        // profile) *before* resolving/spawning the sender — see
        // ensure_reflector's own doc comment for why this ordering matters.
        let target_addr = ensure_reflector(
            runtime_dir,
            fiveg_cfg,
            five_g_plan,
            vxprobe_bin,
            profile,
            port,
        )?;

        if is_up(runtime_dir, &profile.id, "send") {
            return Ok(format!(
                "traffic profile `{}` already up (idempotent)",
                profile.id
            ));
        }
        spawn_tracked(
            runtime_dir,
            &profile.id,
            "send",
            vxprobe_bin,
            &[
                "send".to_string(),
                "--target".to_string(),
                format!("{target_addr}:{port}"),
                "--pps".to_string(),
                pps.to_string(),
            ],
        )?;
        Ok(format!(
            "traffic profile `{}` up (vxprobe send/reflect @ {target_addr}:{port}, {pps} pps)",
            profile.id
        ))
    } else {
        let target_addr = transport_addr(fiveg_cfg, &profile.id, &profile.target)?;
        let bandwidth = profile
            .bandwidth_mbps
            .expect("TrafficPlan guarantees pps or bandwidth_mbps is set");
        if is_up(runtime_dir, &profile.id, "load") {
            return Ok(format!(
                "traffic profile `{}` already up (idempotent)",
                profile.id
            ));
        }
        spawn_tracked(
            runtime_dir,
            &profile.id,
            "load",
            vxtraffic_bin,
            &[
                "--target".to_string(),
                format!("{target_addr}:{port}"),
                "--bandwidth-mbps".to_string(),
                bandwidth.to_string(),
            ],
        )?;
        Ok(format!(
            "traffic profile `{}` up (vxtraffic @ {target_addr}:{port}, {bandwidth} mbit)",
            profile.id
        ))
    }
}

/// Tear one profile's host-spawned processes down. Idempotent: no
/// pidfile/dead pid per role is a no-op. Endpoint interfaces are
/// deliberately left in place — they're `gateway`-owned (or
/// podman-pod-owned for s-nssai), not this module's to remove. An
/// s-nssai target's `ue_probe_up` reflector is torn down by
/// `fiveg::ue_down` (destroys the whole UE pod), not here.
pub fn profile_down(runtime_dir: &Path, profile_id: &str) -> String {
    for role in ["reflect", "send", "load"] {
        if let Some(pid) = read_pid(runtime_dir, profile_id, role) {
            let _ = Command::new("kill").arg(pid.to_string()).output();
            std::thread::sleep(Duration::from_millis(300));
            if alive(pid) {
                let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
            }
        }
        let _ = std::fs::remove_file(pidfile(runtime_dir, profile_id, role));
    }
    format!("traffic profile `{profile_id}` down")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ExperimentManifest;

    fn snssai_profile(id: &str, pps: Option<u64>, bandwidth_mbps: Option<u64>) -> ResolvedProfile {
        ResolvedProfile {
            id: id.to_string(),
            source: ProfileEndpoint::SnssaiUe {
                slice_id: "adas".to_string(),
            },
            target: ProfileEndpoint::SnssaiFarSide,
            pps,
            bandwidth_mbps,
            payload_bytes: 64,
        }
    }

    #[test]
    fn down_is_idempotent_with_no_pidfiles() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            profile_down(tmp.path(), "no-such-profile"),
            "traffic profile `no-such-profile` down"
        );
    }

    #[test]
    fn probe_targeting_bridge_attach_addr_is_rejected() {
        // pps set (a round-trip probe) but the target is SnssaiFarSide (the
        // CN pod's own bridge-attach address) -- nothing reflects there,
        // only a real UE or a vlan endpoint does. Neither endpoint touches
        // Shell/podman (both are s-nssai variants, no-ops in
        // ensure_endpoint), so this exercises purely the dispatch logic.
        let tmp = tempfile::tempdir().unwrap();
        let sh = Shell::default();
        let fiveg_cfg = FiveGLifecycleConfig::default();
        let profile = snssai_profile("bad-probe", Some(1000), None);
        let err = profile_up(
            &sh,
            &fiveg_cfg,
            None,
            tmp.path(),
            "vxprobe",
            "vxtraffic",
            &profile,
            9800,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            TrafficLifecycleError::ProbeTargetNotReflectable(id) if id == "bad-probe"
        ));
    }

    #[test]
    fn snssai_ue_target_without_a_fiveg_plan_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let sh = Shell::default();
        let fiveg_cfg = FiveGLifecycleConfig::default();
        // Target the UE itself (not the far side) so the missing-plan check
        // in ensure_reflector, not ProbeTargetNotReflectable, is what fires.
        let mut profile = snssai_profile("adas-probe", Some(1000), None);
        std::mem::swap(&mut profile.source, &mut profile.target);
        let err = profile_up(
            &sh,
            &fiveg_cfg,
            None,
            tmp.path(),
            "vxprobe",
            "vxtraffic",
            &profile,
            9800,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            TrafficLifecycleError::MissingFiveGSession(profile_id, slice) if profile_id == "adas-probe" && slice == "adas"
        ));
    }

    #[test]
    fn snssai_ue_target_with_a_plan_missing_the_session_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let sh = Shell::default();
        let fiveg_cfg = FiveGLifecycleConfig::default();
        let mut profile = snssai_profile("adas-probe", Some(1000), None);
        std::mem::swap(&mut profile.source, &mut profile.target);

        // A plan with no sessions at all -- the lookup for "adas" fails the
        // same way a missing plan does.
        let manifest = ExperimentManifest::from_yaml(
            r#"
experiment: { id: t, name: t }
network:
  slices:
    - { id: infotainment, mode: s-nssai, s_nssai: { sst: 2 } }
"#,
        )
        .unwrap();
        let plan = FiveGPlan::from_manifest(&manifest);
        // "adas" isn't declared, so this manifest actually has a session
        // for "infotainment" only -- confirms the lookup misses cleanly.
        let plan = plan.unwrap_or_else(|e| panic!("expected a plan: {e:?}"));
        let err = profile_up(
            &sh,
            &fiveg_cfg,
            Some(&plan),
            tmp.path(),
            "vxprobe",
            "vxtraffic",
            &profile,
            9800,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            TrafficLifecycleError::MissingFiveGSession(profile_id, slice) if profile_id == "adas-probe" && slice == "adas"
        ));
    }

    #[test]
    fn load_profile_reports_spawn_failure_for_missing_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let sh = Shell::default();
        let fiveg_cfg = FiveGLifecycleConfig::default();
        let profile = snssai_profile("load", None, Some(400));
        let err = profile_up(
            &sh,
            &fiveg_cfg,
            None,
            tmp.path(),
            "vxprobe",
            "/nonexistent/vxtraffic-binary",
            &profile,
            9900,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            TrafficLifecycleError::Spawn { role: "load", .. }
        ));
    }
}
