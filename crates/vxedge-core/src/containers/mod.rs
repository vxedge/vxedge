//! Generic container provisioning over Podman's REST API (`docs-internal/PROVISIONING.md` §7.2,
//! Track V1). Owned by neither `fiveg` nor `zones`: it is the one place this crate talks to a
//! container engine, and both of those are its callers — `fiveg::lifecycle` (V2, replacing its
//! `podman` CLI calls) and the zone `container` backend's OCI payload (V3).
//!
//! Two layers, same split as every other provisioning module here:
//!
//! - [`spec`] — pure: what a group is ([`GroupSpec`], [`ContainerSpec`]), how its containers
//!   are named and labelled, and what `up` must do given what the daemon reports
//!   ([`infra_action`], [`member_action`]). Unit-tested without a daemon.
//! - [`client`] — [`ContainerClient`], a thin `bollard` edge that observes, then executes what
//!   `spec` decided. No decisions of its own beyond mapping daemon errors to [`ContainerError`].
//!
//! **Rootless user socket only.** The socket identity decides the image store: a different user
//! resolves a different, possibly never-built store, which is the 2026-07-17 incident
//! (`fiveg::lifecycle`'s `host_ip` doc has the account). [`user_socket_path`] refuses to resolve
//! one as root rather than silently pick root's store.
//!
//! What a group shares: the infra container's **network** namespace only
//! (`NetworkMode: container:<id>`), which starts out as a pod's does ([`InfraNetwork::Pasta`])
//! unless the group asks for loopback only ([`InfraNetwork::None`], the zone `container`
//! backend). A `podman pod` also shares IPC and UTS by default; nothing this project runs in a
//! pod relies on either: Open5GS NFs talk over loopback SBI and no rendered config names a host.
//! Confirmed live 2026-10-28 (V2, Ubuntu 26.04, Podman 5.7.0): `fiveg_podman` and
//! `traffic_podman` pass unchanged on groups (NG Setup, PDU sessions on both slices, `vxprobe`
//! round trips at 0% loss through the s-nssai path).

pub mod client;
pub mod spec;

use std::ffi::OsStr;
use std::path::PathBuf;

pub use client::{ContainerClient, GroupHandle, LazyClient, Outcome};
pub use spec::{
    Bind, ContainerSpec, GroupSpec, GroupStatus, InfraAction, InfraNetwork, MemberAction,
    NameError, ObservedContainer, PAUSE_IMAGE, infra_action, member_action,
};

#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    #[error(transparent)]
    Name(#[from] NameError),
    #[error(
        "refusing to resolve the rootless Podman socket as root: root has its own rootless \
         identity and image store, which has never built this project's images. Run vxedged as \
         the user who built them, or pass that user's socket path explicitly"
    )]
    RootSocket,
    #[error(
        "Podman API socket not reachable at `{path}`: {detail}. Enable the rootless user socket, \
         as the user that built the images: `systemctl --user enable --now podman.socket` \
         (docs/TESTBED-SETUP.md)"
    )]
    SocketUnreachable { path: String, detail: String },
    #[error(
        "image `{image}` is not in this user's Podman store and pulling it failed: {detail}. A \
         locally built image (`vx5g-core`, `vx5g-ran`) exists only in the store of the user who \
         built it: build it as the user vxedged runs as (containers/fiveg/README.md)"
    )]
    ImageUnavailable { image: String, detail: String },
    #[error(
        "a container named `{name}` already exists but was not created by vxedge's containers \
         module (it carries no `io.vxedge.group` label), most likely left by an older vxedged \
         that drove the podman CLI. Remove it (`podman rm -f {name}`, or `podman pod rm -f \
         <pod>` for a whole pod) and re-run"
    )]
    NameTaken { name: String },
    #[error("group `{group}` is not up: it has no infra container. Bring the group up first")]
    GroupNotUp { group: String },
    #[error(
        "group `{group}`: infra container is not running, so it has no network namespace to \
         join. Re-run `up`, which rebuilds the group"
    )]
    NoInfraPid { group: String },
    #[error("`{container}` exited with status {code}; output:\n{output}")]
    ExitedNonZero {
        container: String,
        code: i64,
        output: String,
    },
    #[error("{op}: {source}")]
    Api {
        op: String,
        #[source]
        source: bollard::errors::Error,
    },
}

/// The rootless Podman API socket for the user `euid`: `$XDG_RUNTIME_DIR/podman/podman.sock`,
/// falling back to `/run/user/<uid>/…` when the variable is unset (a systemd service without a
/// user session). Pure — the caller passes the uid and environment — so the root refusal is
/// unit-tested without being root.
pub fn user_socket_path(
    euid: u32,
    xdg_runtime_dir: Option<&OsStr>,
) -> Result<PathBuf, ContainerError> {
    if euid == 0 {
        return Err(ContainerError::RootSocket);
    }
    let runtime = match xdg_runtime_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(format!("/run/user/{euid}")),
    };
    Ok(runtime.join("podman").join("podman.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_never_resolves_a_rootless_socket() {
        assert!(matches!(
            user_socket_path(0, Some(OsStr::new("/run/user/0"))),
            Err(ContainerError::RootSocket)
        ));
    }

    #[test]
    fn user_socket_follows_xdg_runtime_dir_then_falls_back() {
        assert_eq!(
            user_socket_path(1000, Some(OsStr::new("/tmp/xdg"))).unwrap(),
            PathBuf::from("/tmp/xdg/podman/podman.sock")
        );
        assert_eq!(
            user_socket_path(1000, None).unwrap(),
            PathBuf::from("/run/user/1000/podman/podman.sock")
        );
        assert_eq!(
            user_socket_path(1000, Some(OsStr::new(""))).unwrap(),
            PathBuf::from("/run/user/1000/podman/podman.sock"),
            "an empty variable is unset, not the current directory"
        );
    }
}
