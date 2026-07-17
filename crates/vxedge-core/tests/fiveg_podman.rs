//! Privileged integration test: the fiveg lifecycle module driving real
//! podman, across two pods (CN + RAN) joined by a transit veth. Needs the
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
//!   podman build -t vx5g-ran -f Containerfile.ueransim .
//!   cd ../.. && sudo -v && \
//!     cargo test -p vxedge-core --test fiveg_podman -- --ignored --nocapture

use vxedge_core::ExperimentManifest;
use vxedge_core::fiveg::{FiveGLifecycleConfig, FiveGPlan, fiveg_down, fiveg_status, fiveg_up};

const MANIFEST: &str = r#"
experiment: { id: fiveg-podman-test, name: fiveg lifecycle integration }
network:
  slices:
    - { id: adas, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }
    - { id: infotainment, mode: s-nssai, s_nssai: { sst: 2, sd: '000002' } }
"#;

/// Removes both test pods even if an assertion panics mid-test.
struct PodGuard(FiveGLifecycleConfig);
impl Drop for PodGuard {
    fn drop(&mut self) {
        let _ = fiveg_down(&self.0);
    }
}

#[test]
#[ignore = "needs root + podman + the vx5g-core/vx5g-ran images (see module docs)"]
fn fiveg_up_brings_cn_and_ran_pods_up_idempotently_then_tears_down() {
    let manifest = ExperimentManifest::from_yaml(MANIFEST).expect("manifest parses");
    let plan = FiveGPlan::from_manifest(&manifest).expect("plans");
    let runtime_dir = tempfile::tempdir().expect("tempdir");
    let cfg = FiveGLifecycleConfig {
        pod_name: "vx5g-cargo-test".to_string(),
        ran_pod_name: "vx5g-cargo-test-ran".to_string(),
        core_image: "vx5g-core".to_string(),
        ran_image: "vx5g-ran".to_string(),
    };
    let _guard = PodGuard(cfg.clone());

    let up_msg = fiveg_up(&plan, runtime_dir.path(), &cfg).expect("fiveg_up");
    assert!(up_msg.contains("6 dependency phases"));
    assert!(up_msg.contains("2 subscribers"));

    let status = fiveg_status(&cfg).expect("fiveg_status");
    assert!(status.up, "CN pod should be up");
    assert!(status.ran_up, "RAN pod should be up");
    // CN: 8 core NFs + mongo + 2 slices x (smf, upf) = 13 containers.
    assert_eq!(status.containers.len(), 13, "{:?}", status.containers);
    // RAN: gNB only (UEs are started separately per test run).
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
    let second = fiveg_up(&plan, runtime_dir.path(), &cfg).expect("fiveg_up idempotent");
    assert!(second.contains("already up"));

    let down_msg = fiveg_down(&cfg).expect("fiveg_down");
    assert!(down_msg.contains("down"));
    let status_after = fiveg_status(&cfg).expect("fiveg_status after down");
    assert!(!status_after.up);
    assert!(!status_after.ran_up);

    // Idempotent down: tearing down already-absent pods doesn't error.
    fiveg_down(&cfg).expect("fiveg_down idempotent on absent pods");
}
