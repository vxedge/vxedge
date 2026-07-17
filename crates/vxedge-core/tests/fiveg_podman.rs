//! Privileged integration test: the fiveg lifecycle module driving real
//! podman. Needs the `vx5g-core` image built (`containers/fiveg/`) and
//! network access for the first `mongo:7` pull. Ignored by default.
//!
//! Run with:
//!   cd vxedge/containers/fiveg && podman build -t vx5g-core -f Containerfile.open5gs .
//!   cargo test -p vxedge-core --test fiveg_podman -- --ignored --nocapture

use vxedge_core::ExperimentManifest;
use vxedge_core::fiveg::{FiveGLifecycleConfig, FiveGPlan, fiveg_down, fiveg_status, fiveg_up};

const MANIFEST: &str = r#"
experiment: { id: fiveg-podman-test, name: fiveg lifecycle integration }
network:
  slices:
    - { id: adas, mode: s-nssai, s_nssai: { sst: 1, sd: '000001' } }
    - { id: infotainment, mode: s-nssai, s_nssai: { sst: 2, sd: '000002' } }
"#;

/// Removes the test pod even if an assertion panics mid-test.
struct PodGuard(FiveGLifecycleConfig);
impl Drop for PodGuard {
    fn drop(&mut self) {
        let _ = fiveg_down(&self.0);
    }
}

#[test]
#[ignore = "needs podman + the vx5g-core image (see module docs)"]
fn fiveg_up_brings_a_running_pod_up_idempotently_then_tears_down() {
    let manifest = ExperimentManifest::from_yaml(MANIFEST).expect("manifest parses");
    let plan = FiveGPlan::from_manifest(&manifest).expect("plans");
    let runtime_dir = tempfile::tempdir().expect("tempdir");
    let cfg = FiveGLifecycleConfig {
        pod_name: "vx5g-cargo-test".to_string(),
        core_image: "vx5g-core".to_string(),
    };
    let _guard = PodGuard(cfg.clone());

    let up_msg = fiveg_up(&plan, runtime_dir.path(), &cfg).expect("fiveg_up");
    assert!(up_msg.contains("6 dependency phases"));
    assert!(up_msg.contains("2 subscribers"));

    let status = fiveg_status(&cfg).expect("fiveg_status");
    assert!(status.up);
    // 8 core NFs + mongo + 2 slices x (smf, upf) = 13 containers.
    assert_eq!(status.containers.len(), 13, "{:?}", status.containers);
    assert!(
        status.all_running(),
        "expected every container running, got {:?}",
        status.containers
    );

    // Idempotent up: second call is a no-op, doesn't error or duplicate.
    let second = fiveg_up(&plan, runtime_dir.path(), &cfg).expect("fiveg_up idempotent");
    assert!(second.contains("already up"));

    let down_msg = fiveg_down(&cfg).expect("fiveg_down");
    assert!(down_msg.contains("down"));
    let status_after = fiveg_status(&cfg).expect("fiveg_status after down");
    assert!(!status_after.up);

    // Idempotent down: tearing down an already-absent pod doesn't error.
    fiveg_down(&cfg).expect("fiveg_down idempotent on absent pod");
}
