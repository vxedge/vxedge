//! Privileged integration test: the *actual* H2-mechanism scenario proven
//! live on 2026-07-17 (`docs/STATUS.md`'s "TrafficService live end to end"
//! entry) — a manifest with two `mode: vlan` slices and one `mode: s-nssai`
//! slice, all three `traffic_profiles` running *simultaneously* through the
//! real production path (`gateway::provision::apply*` + `fiveg` +
//! `traffic::lifecycle`, the same sequence `vxedged`'s `GatewaySvc`/
//! `TrafficSvc` run — see `crates/vxedged/src/services.rs`), not a
//! throwaway test bridge. This is the permanent, runnable replacement for
//! the ad-hoc `verify_traffic_service.sh` shell script used to find the
//! bugs this scenario now guards against (same-VLAN collision, s-nssai
//! validation ordering, and — the two big ones — the CN pod's own bridge
//! port having VLAN membership without the *bridge itself* or the *host*
//! having any way to reach it).
//!
//! Needs the `vx5g-core`/`vx5g-ran` images (`containers/fiveg/`, the latter
//! built via `containers/fiveg/build-vx5g-ran.sh` so it bundles `vxprobe`)
//! and release `vxprobe`/`vxtraffic` binaries from the sibling `vxagents`
//! repo. Ignored by default.
//!
//! **Do not run under `sudo`** — same reason as `fiveg_podman.rs`: it would
//! switch `podman` to root's own separate rootless identity, which never
//! built these images. Cache a sudo credential first so `Shell`'s internal
//! escalated calls (bridge/vlan/route) don't block on a prompt.
//!
//! Run with:
//!   cd vxedge/containers/fiveg
//!   podman build -t vx5g-core -f Containerfile.open5gs .
//!   ./build-vx5g-ran.sh
//!   cd ../../../vxagents && cargo build --release -p vxprobe -p vxtraffic
//!   cd ../vxedge && sudo -v && \
//!     VXPROBE_BIN=../vxagents/target/release/vxprobe \
//!     VXTRAFFIC_BIN=../vxagents/target/release/vxtraffic \
//!     cargo test -p vxedge-core --test traffic_podman -- --ignored --nocapture

use vxedge_core::fiveg::{self, FiveGLifecycleConfig, FiveGPlan};
use vxedge_core::gateway::{self, GatewayPlan, Shell};
use vxedge_core::traffic::{TrafficPlan, profile_down, profile_up};

const PROBE_PORT: u16 = 9800;

const WORKSPACE: &str = r#"
kind: experiment
id: traffic-podman-test
vehicles: [veh]
network:
  defaults: { bandwidth: 500000000 }
  slices:
    - { id: adas, mode: vlan, vlan: 101, qos: { min_rate_mbps: 100, max_rate_mbps: 200, priority: 1 } }
    - { id: infotainment, mode: vlan, vlan: 103, qos: { min_rate_mbps: 0, max_rate_mbps: 400, priority: 5 } }
    - { id: h2slice, mode: s-nssai, s_nssai: { sst: 3, sd: '000003' } }
traffic_profiles:
  adas-probe:
    source: node-adas-01
    target: internet
    pps: 200
  infotainment-load:
    source: node-info-01
    target: internet
    bandwidth_mbps: 100
  h2-probe:
    source: internet
    target: node-h2-01
    pps: 200
---
kind: vehicle
id: veh
devices:
  node-adas-01: { role: ecu, backend: container }
  node-info-01: { role: ecu, backend: container }
  node-h2-01:   { role: ecu, backend: container }
links:
  - { id: l-adas,  endpoints: [node-adas-01:eth0], slice: adas }
  - { id: l-infot, endpoints: [node-info-01:eth0], slice: infotainment }
  - { id: l-h2,    endpoints: [node-h2-01:eth0],   slice: h2slice }
"#;

fn vxprobe_bin() -> String {
    std::env::var("VXPROBE_BIN").unwrap_or_else(|_| "vxprobe".to_string())
}
fn vxtraffic_bin() -> String {
    std::env::var("VXTRAFFIC_BIN").unwrap_or_else(|_| "vxtraffic".to_string())
}

/// Tears everything down in the same reverse order `GatewaySvc::Down` does
/// (UE pods -> 5G core/uplink attachment -> bridge/endpoints), even if an
/// assertion panics mid-test.
struct Guard {
    sh: Shell,
    gateway_plan: GatewayPlan,
    fiveg_cfg: FiveGLifecycleConfig,
    five_g_plan: FiveGPlan,
    profile_ids: Vec<String>,
    runtime_dir: std::path::PathBuf,
}
impl Drop for Guard {
    fn drop(&mut self) {
        for id in &self.profile_ids {
            profile_down(&self.runtime_dir, id);
        }
        for session in &self.five_g_plan.sessions {
            let _ = fiveg::ue_down(&self.fiveg_cfg, session);
        }
        let _ = fiveg::bridge_down(&self.fiveg_cfg);
        let _ = gateway::provision::teardown_fiveg_uplink(&self.sh, &self.gateway_plan.bridge);
        let _ = fiveg::fiveg_down(&self.fiveg_cfg);
        let _ = gateway::teardown(&self.sh, &self.gateway_plan);
    }
}

#[test]
#[ignore = "needs root + podman + vx5g-core/vx5g-ran images + built vxprobe/vxtraffic (see module docs)"]
fn traffic_service_runs_vlan_and_snssai_profiles_simultaneously_through_the_real_bridge() {
    let model = vxedge_core::model::load_yaml(WORKSPACE).expect("workspace loads");
    let gateway_plan = GatewayPlan::from_model(&model).expect("gateway plan");
    let five_g_plan = FiveGPlan::from_model(&model).expect("fiveg plan");
    let traffic_plan = TrafficPlan::from_model(&model, &gateway_plan).expect("traffic plan");
    assert_eq!(traffic_plan.profiles.len(), 3);

    let sh = Shell::default();
    let fiveg_cfg = FiveGLifecycleConfig::default();
    let runtime_dir = tempfile::tempdir().expect("tempdir");

    let mut guard = Guard {
        sh: sh.clone(),
        gateway_plan: gateway_plan.clone(),
        fiveg_cfg: fiveg_cfg.clone(),
        five_g_plan: five_g_plan.clone(),
        profile_ids: vec![],
        runtime_dir: runtime_dir.path().to_path_buf(),
    };

    // 1. vxgateway up — the same three-phase sequence GatewaySvc::Up runs.
    gateway::provision::apply_links(&sh, &gateway_plan).expect("apply_links");
    gateway::provision::apply_slices(&sh, &gateway_plan).expect("apply_slices");
    gateway::provision::apply_endpoints(&sh, &gateway_plan).expect("apply_endpoints");
    let obs = gateway::observe(&sh, &gateway_plan).expect("observe");
    assert!(obs.fully_up(), "gateway should be fully up: {obs:?}");

    // 2. 5G core up, attached to the real bridge — the exact sequence that
    // needed two extra real fixes (bridge `self` VLAN membership, UE
    // subnet routes) to actually work end to end.
    fiveg::fiveg_up(&five_g_plan, runtime_dir.path(), &fiveg_cfg).expect("fiveg_up");
    let cn_pid = fiveg::pod_pid(&fiveg_cfg.pod_name).expect("cn pod pid");
    fiveg::bridge_up(&fiveg_cfg, &gateway_plan.bridge, cn_pid).expect("bridge_up");
    let host_veth = fiveg::bridge_host_veth(&fiveg_cfg.pod_name);
    let host_iface =
        gateway::provision::apply_fiveg_uplink(&sh, &host_veth).expect("apply_fiveg_uplink");
    let ue_subnets: Vec<String> = five_g_plan
        .sessions
        .iter()
        .map(|s| s.ue_subnet.clone())
        .collect();
    gateway::provision::apply_fiveg_ue_routes(&sh, &host_iface, &ue_subnets)
        .expect("apply_fiveg_ue_routes");

    let ran_pid = fiveg::pod_pid(&fiveg_cfg.ran_pod_name).expect("ran pod pid");
    let cfg_dir = runtime_dir.path().join("fiveg-config");
    for (i, session) in five_g_plan.sessions.iter().enumerate() {
        fiveg::ue_up(&fiveg_cfg, &cfg_dir, ran_pid, i, session).expect("ue_up");
    }

    // 3. traffic up — all three profiles simultaneously, through the real
    // path. This is the assertion that actually matters: it's the thing
    // that was 100% loss twice before the two host-reachability fixes.
    let vxprobe = vxprobe_bin();
    let vxtraffic = vxtraffic_bin();
    for profile in &traffic_plan.profiles {
        let msg = profile_up(
            &sh,
            &fiveg_cfg,
            Some(&five_g_plan),
            runtime_dir.path(),
            &vxprobe,
            &vxtraffic,
            profile,
            PROBE_PORT,
            12,
        )
        .unwrap_or_else(|e| panic!("profile_up(`{}`) failed: {e}", profile.id));
        assert!(msg.contains("up"), "{msg}");
        guard.profile_ids.push(profile.id.clone());
    }

    // Idempotent up: a second call per profile is a no-op, doesn't spawn a
    // duplicate process.
    for profile in &traffic_plan.profiles {
        let msg = profile_up(
            &sh,
            &fiveg_cfg,
            Some(&five_g_plan),
            runtime_dir.path(),
            &vxprobe,
            &vxtraffic,
            profile,
            PROBE_PORT,
            12,
        )
        .expect("profile_up idempotent");
        assert!(msg.contains("already up"), "{msg}");
    }

    // Give the bounded `vxprobe send`/`vxtraffic` runs time to finish and
    // write their JSON summaries, then check every one reported success —
    // not just "the process started", the actual proof this scenario
    // exists for: real packets round-tripped with 0% loss through the real
    // bridge, for both slice modes, at the same time.
    std::thread::sleep(std::time::Duration::from_secs(12));
    for profile in &traffic_plan.profiles {
        let role = if profile.pps.is_some() {
            "send"
        } else {
            "load"
        };
        let log_path = runtime_dir
            .path()
            .join(format!("traffic-{}-{role}.log", profile.id));
        let log = std::fs::read_to_string(&log_path)
            .unwrap_or_else(|e| panic!("reading {log_path:?}: {e}"));
        assert!(
            !log.trim().is_empty(),
            "profile `{}` ({role}) produced no output",
            profile.id
        );
        if profile.pps.is_some() {
            assert!(
                log.contains("\"loss_pct\":0") || log.contains("\"loss_pct\": 0"),
                "profile `{}` should show 0% loss, got: {log}",
                profile.id
            );
        }
    }

    // 4. traffic down, then the rest of the guard's teardown runs via Drop
    // — assert it's clean here too rather than only relying on Drop's
    // best-effort cleanup.
    for id in guard.profile_ids.drain(..) {
        let msg = profile_down(runtime_dir.path(), &id);
        assert!(msg.contains("down"), "{msg}");
    }
}
