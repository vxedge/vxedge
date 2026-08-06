//! Proves `zones::node_up`'s new registry-fetch wiring against a real, published, digest-pinned
//! part — `ghcr.io/vxedge/parts/zone/linux/stack/probe-bundle:0.1.0` (`vxparts/TASKS.md`'s own
//! first end-to-end proof-of-concept: "built → pushed → pulled fresh from the registry →
//! booted the pulled artifact under `qemu-microvm`", 2026-07-26). This is that proof driven
//! through `vxedge-core`'s real `node_up` code path instead of a manual `oras pull` + manual
//! `qemu-system-x86_64` invocation.
//!
//! Deliberately does **not** attempt a live-networked boot: that needs a real gateway
//! (`vxedge up`) to have already created the tap this `NodePlan` names, which this test does
//! not stand up — doing so touches shared host bridge/tap state, out of scope for a focused
//! fetch-wiring proof. What this test asserts instead: the fetch succeeds, the cache directory
//! ends up holding a real `vmlinuz`/`initrd.img`, and `node_up` gets *past* fetch-and-bundle
//! resolution before failing (if it fails at all) — any failure must be the tap not existing,
//! never `ZoneBackendError::Fetch`/`BundleFile`.
//!
//! Ignored by default (needs network + the real `oras` CLI + GHCR access). Run with:
//!   cargo test -p vxedge-core --test registry_fetch_boot -- --ignored

#![cfg(target_os = "linux")]

use std::path::PathBuf;

use vxedge_core::gateway::Shell;
use vxedge_core::model::{Backend, DeviceRole};
use vxedge_core::zones::backend::{ZoneBackendError, node_up};
use vxedge_core::zones::plan::NodePlan;

/// The exact part this session's `vxparts` milestone published and boot-tested — re-verified
/// live via `oras resolve` before writing this test, not copied from memory.
const REFERENCE: &str = "ghcr.io/vxedge/parts/zone/linux/stack/probe-bundle";
const DIGEST: &str = "sha256:33c9c8d8625bd5c6d1f4d3d0c561fdaa930e057fcd1fb24fb3badb402f25429d";

fn scratch_dirs(name: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "vx-registry-fetch-boot-{name}-{}",
        std::process::id()
    ));
    let runtime_dir = base.join("runtime");
    let cache_dir = base.join("cache");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&cache_dir).unwrap();
    (runtime_dir, cache_dir)
}

fn remote_plan() -> NodePlan {
    NodePlan {
        zone_id: "front-left".into(),
        node_id: "node-registry-fetch-test".into(),
        role: DeviceRole::Ecu,
        backend: Backend::QemuMicrovm,
        // Deliberately a tap that does not exist — see module doc. `microvm_up` is expected to
        // fail *here*, after fetch/bundle resolution has already succeeded.
        endpoint: "vx-nonexistent-tap".into(),
        peer: "vx-nonexistent-tap".into(),
        kind: vxedge_core::gateway::plan::EndpointKind::Tap,
        vlan: 101,
        slice: Some("adas".into()),
        ip_cidr: None,
        bundle: None,
        remote_bundle: Some((REFERENCE.to_string(), DIGEST.to_string())),
        pinned_cpus: Vec::new(),
    }
}

#[test]
#[ignore = "needs network + real oras CLI + GHCR access; run with --ignored"]
fn fetches_the_real_published_probe_bundle_before_attempting_boot() {
    let (runtime_dir, cache_dir) = scratch_dirs("fetch");
    let sh = Shell {
        netns: None,
        escalate: false,
    };
    let plan = remote_plan();

    let result = node_up(&sh, &plan, &runtime_dir, &cache_dir);

    // The fetch must have happened and produced real files — checked directly, not inferred
    // from `node_up`'s own return value (which is allowed to fail downstream, at the
    // tap-attach step this test intentionally doesn't provide).
    let pulled_dir = cache_dir.join(DIGEST.trim_start_matches("sha256:"));
    assert!(
        pulled_dir.join("vmlinuz").is_file(),
        "expected a real vmlinuz fetched into {pulled_dir:?}"
    );
    assert!(
        pulled_dir.join("initrd.img").is_file(),
        "expected a real initrd.img fetched into {pulled_dir:?}"
    );

    // Whatever `node_up` returns, it must not be a fetch/bundle-resolution failure — those are
    // exactly the two error shapes this test exists to rule out.
    if let Err(e) = &result {
        assert!(
            !matches!(
                e,
                ZoneBackendError::Fetch { .. } | ZoneBackendError::BundleFile { .. }
            ),
            "node_up failed at fetch/bundle resolution, not just the (expected, out-of-scope) \
             missing-tap step: {e}"
        );
    }

    let _ = std::fs::remove_dir_all(runtime_dir.parent().unwrap());
}

#[test]
#[ignore = "needs network + real oras CLI + GHCR access; run with --ignored"]
fn a_second_fetch_is_a_cache_hit_not_a_re_pull() {
    let (runtime_dir, cache_dir) = scratch_dirs("idempotent");
    let sh = Shell {
        netns: None,
        escalate: false,
    };
    let plan = remote_plan();

    let _ = node_up(&sh, &plan, &runtime_dir, &cache_dir);
    let pulled_dir = cache_dir.join(DIGEST.trim_start_matches("sha256:"));
    let vmlinuz_mtime_1 = std::fs::metadata(pulled_dir.join("vmlinuz"))
        .expect("first fetch populated the cache")
        .modified()
        .unwrap();

    // A second `node_up` over the same cache_dir must not re-pull: `registry::fetch`'s own
    // short-circuit (`cache_path` already a dir) is what makes `zone up` idempotent for a
    // published part, the same contract every backend already carries for local ones.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let _ = node_up(&sh, &plan, &runtime_dir, &cache_dir);
    let vmlinuz_mtime_2 = std::fs::metadata(pulled_dir.join("vmlinuz"))
        .expect("still present after the second call")
        .modified()
        .unwrap();

    assert_eq!(
        vmlinuz_mtime_1, vmlinuz_mtime_2,
        "second node_up re-fetched instead of hitting the cache"
    );

    let _ = std::fs::remove_dir_all(runtime_dir.parent().unwrap());
}
