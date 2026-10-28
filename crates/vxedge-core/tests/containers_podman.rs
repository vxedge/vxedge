//! Live test of `vxedge_core::containers` against a real Podman API socket: stand a group up,
//! check its member really shares the infra container's network namespace, re-run `up` and
//! confirm nothing changed, run a one-shot to completion inside the group, then tear down
//! twice. Needs no root and no project images — only a public `alpine` pull. Ignored by
//! default.
//!
//! **Do not run under `sudo`**: the rootless socket of root is a different image store (see
//! `containers`' module doc), and `ContainerClient::connect_user` refuses it.
//!
//! Run with the user socket enabled (`systemctl --user enable --now podman.socket`):
//!   cargo test -p vxedge-core --test containers_podman -- --ignored --nocapture
//! or against any other Podman API socket (a `podman machine` on macOS, for instance):
//!   VXEDGE_PODMAN_SOCKET=<path> cargo test -p vxedge-core --test containers_podman -- --ignored

use std::path::PathBuf;

use vxedge_core::containers::{ContainerClient, ContainerError, ContainerSpec, GroupSpec, Outcome};

/// A public image small enough to pull in seconds. Pinned by tag only: this is a fixture for a
/// mechanism test, not a measured experiment artifact, and no campaign number depends on it.
const IMAGE: &str = "docker.io/library/alpine:3.20";
const GROUP: &str = "vxedge-containers-test";

async fn client() -> ContainerClient {
    match std::env::var_os("VXEDGE_PODMAN_SOCKET") {
        Some(p) => ContainerClient::connect(&PathBuf::from(p)).await,
        None => ContainerClient::connect_user().await,
    }
    .expect("a reachable Podman API socket (see module docs)")
}

fn group() -> GroupSpec {
    GroupSpec::new(
        GROUP,
        ContainerSpec {
            image: IMAGE.into(),
            cmd: vec!["sleep".into(), "infinity".into()],
            ..Default::default()
        },
    )
    .unwrap()
}

fn member() -> ContainerSpec {
    ContainerSpec {
        name: "worker".into(),
        image: IMAGE.into(),
        cmd: vec!["sleep".into(), "infinity".into()],
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "needs a Podman API socket and network access to pull alpine (see module docs)"]
async fn group_up_is_idempotent_shares_a_netns_and_tears_down_cleanly() {
    let c = client().await;
    let g = group();
    c.group_down(GROUP).await.unwrap(); // clean slate from an earlier aborted run

    // First up creates.
    let (h, outcome) = c.ensure_group(&g).await.unwrap();
    assert_eq!(outcome, Outcome::Created);
    assert!(h.infra_pid > 0);
    assert_eq!(
        c.ensure_member(&g, &h, &member()).await.unwrap(),
        Outcome::Created
    );

    // Second up observes and keeps — the re-run path.
    let (h2, outcome) = c.ensure_group(&g).await.unwrap();
    assert_eq!(outcome, Outcome::Kept);
    assert_eq!(h2, h, "same infra container, same namespace");
    assert_eq!(
        c.ensure_member(&g, &h2, &member()).await.unwrap(),
        Outcome::Kept
    );

    let status = c.group_status(GROUP).await.unwrap();
    assert!(status.all_running(), "{status:?}");
    assert_eq!(status.members.len(), 1);

    // Two separate one-shots in the group land in one network namespace, and — where this test
    // runs on the same kernel as the daemon, so the infra PID is meaningful here — it is the
    // infra container's. (On a `podman machine` the PID belongs to the VM, so only the first
    // half is checkable from the host.)
    let netns = ContainerSpec {
        name: "probe".into(),
        image: IMAGE.into(),
        cmd: vec!["readlink".into(), "/proc/self/ns/net".into()],
        ..Default::default()
    };
    let first = c.run_to_completion(&g, &h, &netns).await.unwrap();
    let second = c.run_to_completion(&g, &h, &netns).await.unwrap();
    assert!(first.trim().starts_with("net:["), "{first:?}");
    assert_eq!(first, second, "both one-shots joined one namespace");
    // On the user socket the daemon is this kernel's, so the check is required, not skipped.
    let same_kernel = std::env::var_os("VXEDGE_PODMAN_SOCKET").is_none();
    match std::fs::read_link(format!("/proc/{}/ns/net", h.infra_pid)) {
        Ok(infra) => assert_eq!(
            first.trim(),
            infra.display().to_string(),
            "and it is the infra container's"
        ),
        Err(e) if same_kernel => panic!("cannot read infra PID {}'s netns: {e}", h.infra_pid),
        Err(_) => eprintln!("infra PID is not on this kernel; netns identity not checked"),
    }

    // A failing one-shot is an error carrying its output, and leaves nothing behind.
    let failing = ContainerSpec {
        name: "fails".into(),
        image: IMAGE.into(),
        cmd: vec!["sh".into(), "-c".into(), "echo boom; exit 3".into()],
        ..Default::default()
    };
    match c.run_to_completion(&g, &h, &failing).await {
        Err(ContainerError::ExitedNonZero { code, output, .. }) => {
            assert_eq!(code, 3);
            assert!(output.contains("boom"), "{output:?}");
        }
        other => panic!("expected ExitedNonZero, got {other:?}"),
    }
    assert_eq!(c.group_status(GROUP).await.unwrap().members.len(), 1);

    // A changed spec converges by recreating that member only.
    let changed = ContainerSpec {
        cmd: vec!["sleep".into(), "3600".into()],
        ..member()
    };
    assert_eq!(
        c.ensure_member(&g, &h, &changed).await.unwrap(),
        Outcome::Recreated
    );

    // Down removes everything; a second down is a no-op.
    assert_eq!(c.group_down(GROUP).await.unwrap(), 2);
    assert_eq!(c.group_down(GROUP).await.unwrap(), 0);
    assert!(!c.group_status(GROUP).await.unwrap().exists());
    assert!(matches!(
        c.handle(GROUP).await,
        Err(ContainerError::GroupNotUp { .. })
    ));
}
