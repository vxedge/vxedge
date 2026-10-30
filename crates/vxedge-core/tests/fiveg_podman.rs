//! Privileged integration test: the fiveg lifecycle module driving real
//! podman (its rootless user API socket, `systemctl --user enable --now
//! podman.socket`), across two pods (CN + RAN) joined by a transit veth. Needs the
//! `vx5g-core` and `vx5g-ran` images built (`containers/fiveg/`) and
//! network access for the first `mongo:7` pull. Ignored by default.
//!
//! **Do not run the test process itself under `sudo`** — `podman` must
//! stay unprivileged (found live, 2026-07-17: `sudo cargo test` switches
//! `podman` to root's own separate rootless-podman identity/storage, which
//! never built these images and fails resolving them). Host networking
//! (the transit veth, entering pod netns) needs CAP_NET_ADMIN regardless,
//! so `lifecycle`'s `host_ip`/`nsenter_ip` shell out to `sudo ip`/
//! `sudo nsenter` internally, scoped to just those calls — cache a sudo
//! credential first so those inner calls don't block on a prompt:
//!
//! Run with:
//!   cd vxedge/containers/fiveg
//!   podman build -t vx5g-core -f Containerfile.open5gs .
//!   ./build-vx5g-ran.sh   # bundles vxprobe from ../../../vxagents
//!   cd ../.. && sudo -v && \
//!     cargo test -p vxedge-core --test fiveg_podman -- --ignored --nocapture

use vxedge_core::containers::ContainerClient;
use vxedge_core::fiveg::{
    FiveGLifecycleConfig, FiveGPlan, SliceSession, fiveg_down, fiveg_status, fiveg_up, pod_exists,
    pod_pid, ue_down, ue_pod_name, ue_probe_up, ue_up,
};

const PROBE_PORT: u16 = 9800;

const WORKSPACE: &str = r#"
kind: experiment
id: fiveg-podman-test
vehicles: [veh]
network:
  slices:
    - { id: adas, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }
    - { id: infotainment, mode: s-nssai, s_nssai: { sst: 2, sd: '000002' } }
---
kind: vehicle
id: veh
devices: {}
"#;

/// Removes both test pods, plus any UE pods brought up, even if an
/// assertion panics mid-test. `Drop` cannot await, so it blocks this
/// (multi-threaded) runtime's worker for the teardown.
struct PodGuard {
    client: ContainerClient,
    cfg: FiveGLifecycleConfig,
    ue_sessions: Vec<SliceSession>,
}
impl Drop for PodGuard {
    fn drop(&mut self) {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                for session in &self.ue_sessions {
                    let _ = ue_down(&self.client, &self.cfg, session).await;
                }
                let _ = fiveg_down(&self.client, &self.cfg).await;
            })
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs root + podman + the vx5g-core/vx5g-ran images (see module docs)"]
async fn fiveg_up_brings_cn_and_ran_pods_up_idempotently_then_tears_down() {
    let client = ContainerClient::connect_user()
        .await
        .expect("the rootless Podman API socket (see module docs)");
    let model = vxedge_core::model::load_yaml(WORKSPACE).expect("workspace loads");
    let plan = FiveGPlan::from_model(&model).expect("plans");
    let runtime_dir = tempfile::tempdir().expect("tempdir");
    let cfg = FiveGLifecycleConfig {
        pod_name: "vx5g-cargo-test".to_string(),
        ran_pod_name: "vx5g-cargo-test-ran".to_string(),
        core_image: "vx5g-core".to_string(),
        ran_image: "vx5g-ran".to_string(),
        ..FiveGLifecycleConfig::default()
    };
    let mut guard = PodGuard {
        client: client.clone(),
        cfg: cfg.clone(),
        ue_sessions: vec![],
    };

    let up_msg = fiveg_up(&client, &plan, runtime_dir.path(), &cfg)
        .await
        .expect("fiveg_up");
    assert!(up_msg.contains("6 dependency phases"));
    assert!(up_msg.contains("2 subscribers"));

    let status = fiveg_status(&client, &cfg).await.expect("fiveg_status");
    assert!(status.up, "CN pod should be up");
    assert!(status.ran_up, "RAN pod should be up");
    // CN: 8 core NFs + mongo + 2 slices x (smf, upf) = 13 containers.
    assert_eq!(status.containers.len(), 13, "{:?}", status.containers);
    // RAN: gNB only (UEs now run in their own pods, not this one).
    assert_eq!(
        status.ran_containers.len(),
        1,
        "{:?}",
        status.ran_containers
    );
    assert!(
        status.all_running(),
        "expected every container in both pods running, got cn={:?} ran={:?}",
        status.containers,
        status.ran_containers
    );

    // Idempotent up: second call is a no-op, doesn't error or duplicate.
    let second = fiveg_up(&client, &plan, runtime_dir.path(), &cfg)
        .await
        .expect("fiveg_up idempotent");
    assert!(second.contains("already up"));

    // Bring both UEs up, each in its own pod — the actual fix under test:
    // UERANSIM's per-UE routing-table-1000 collision only happens when UEs
    // share a netns, so proving each UE lands in its own pod (and that
    // fiveg_up's pre-existing RAN pod stays gNB-only) is the acceptance
    // bar here, not just "some container started".
    let ran_pid = pod_pid(&client, &cfg.ran_pod_name)
        .await
        .expect("ran pod pid");
    let cfg_dir = runtime_dir.path().join("fiveg-config");
    for (i, session) in plan.sessions.iter().enumerate() {
        let msg = ue_up(&client, &cfg, &cfg_dir, ran_pid, i, session)
            .await
            .expect("ue_up");
        assert!(msg.contains("up"));
        guard.ue_sessions.push(session.clone());
        let pod = ue_pod_name(&cfg.ran_pod_name, &session.slice_id);
        assert!(
            pod_exists(&client, &pod).await.expect("pod_exists"),
            "UE pod `{pod}` should exist after ue_up"
        );
    }
    // Idempotent ue_up: second call per UE is a no-op.
    for (i, session) in plan.sessions.iter().enumerate() {
        let msg = ue_up(&client, &cfg, &cfg_dir, ran_pid, i, session)
            .await
            .expect("ue_up idempotent");
        assert!(msg.contains("already up"));
    }

    // vxprobe reflector, inside each UE's own pod — the H2 probe data path.
    for session in &plan.sessions {
        let msg = ue_probe_up(&client, &cfg, &cfg_dir, session, PROBE_PORT)
            .await
            .expect("ue_probe_up");
        assert!(msg.contains("up"));
    }
    // Idempotent ue_probe_up: second call per UE is a no-op.
    for session in &plan.sessions {
        let msg = ue_probe_up(&client, &cfg, &cfg_dir, session, PROBE_PORT)
            .await
            .expect("ue_probe_up idempotent");
        assert!(msg.contains("already up"));
    }

    for session in &plan.sessions {
        let msg = ue_down(&client, &cfg, session).await.expect("ue_down");
        assert!(msg.contains("down"));
        let pod = ue_pod_name(&cfg.ran_pod_name, &session.slice_id);
        assert!(
            !pod_exists(&client, &pod)
                .await
                .expect("pod_exists after ue_down"),
            "UE pod `{pod}` should be gone after ue_down"
        );
    }
    guard.ue_sessions.clear();

    let down_msg = fiveg_down(&client, &cfg).await.expect("fiveg_down");
    assert!(down_msg.contains("down"));
    let status_after = fiveg_status(&client, &cfg)
        .await
        .expect("fiveg_status after down");
    assert!(!status_after.up);
    assert!(!status_after.ran_up);

    // Idempotent down: tearing down already-absent pods doesn't error.
    fiveg_down(&client, &cfg)
        .await
        .expect("fiveg_down idempotent on absent pods");
}
