//! `attach` zone backend: a guest that brings its own host stack (ARCHITECTURE.md §9).
//!
//! Some guests boot only on their platform's own stack, as a production AAOS guest needs its
//! hypervisor vendor's board support. vxedge does not rebuild that stack: the device's part is an
//! adapter directory whose `attach.yaml` says how the platform starts, reports and stops, and the
//! gateway has already adopted the interface the platform created as this node's port
//! (`gateway::plan::EndpointKind::Adopted`). This backend only runs the adapter.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use super::backend::{NodeState, ZoneBackend, ZoneBackendError};
use super::plan::NodePlan;
use crate::gateway::Shell;

/// The adapter a part declares in `attach.yaml`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Adapter {
    /// The host interface the platform creates; must be the one the link endpoint adopts.
    pub interface: String,
    /// Brings the platform up. Runs once per `up` that finds the node down.
    pub start: String,
    /// Exits 0 while the platform is up, non-zero otherwise. Never changes anything.
    pub status: String,
    /// Brings the platform down. Acts on this node only, never host-wide.
    pub stop: String,
    /// How long `start` may take, in seconds: a platform's first boot can take minutes.
    #[serde(default = "default_start_timeout")]
    pub start_timeout_s: u64,
}

fn default_start_timeout() -> u64 {
    600
}

/// How long `status` and `stop` may take.
const STATUS_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) struct AttachBackend;

fn err(np: &NodePlan, detail: impl Into<String>) -> ZoneBackendError {
    ZoneBackendError::Runtime {
        node: np.node_id.clone(),
        detail: detail.into(),
    }
}

fn adapter_dir(np: &NodePlan) -> Result<&Path, ZoneBackendError> {
    np.bundle.as_deref().ok_or_else(|| {
        err(
            np,
            "an attach node needs a local `kind: attach` part, its adapter",
        )
    })
}

/// Reads and checks a node's adapter.
pub fn load(np: &NodePlan) -> Result<(PathBuf, Adapter), ZoneBackendError> {
    let dir = adapter_dir(np)?.to_path_buf();
    let path = dir.join("attach.yaml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| err(np, format!("cannot read {}: {e}", path.display())))?;
    let adapter: Adapter = serde_yaml_ng::from_str(&text).map_err(|e| {
        err(
            np,
            format!("{} is not a valid adapter: {e}", path.display()),
        )
    })?;
    if adapter.interface != np.endpoint {
        return Err(err(
            np,
            format!(
                "the adapter's interface `{}` is not the one the link endpoint adopts (`{}`)",
                adapter.interface, np.endpoint
            ),
        ));
    }
    Ok((dir, adapter))
}

/// Runs one adapter command through `sh -c` in the adapter's directory, with the node's identity
/// in the environment, and waits at most `timeout`.
fn run(
    np: &NodePlan,
    dir: &Path,
    runtime_dir: &Path,
    what: &str,
    script: &str,
    timeout: Duration,
) -> Result<bool, ZoneBackendError> {
    let log = runtime_dir.join(format!("{}.attach.log", np.runtime_name()));
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| err(np, format!("cannot open {}: {e}", log.display())))?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .env("VX_NODE", &np.node_id)
        .env("VX_IP", np.ip_cidr.as_deref().unwrap_or(""))
        .env("VX_INTERFACE", &np.endpoint)
        .env("VX_RUNTIME_DIR", runtime_dir)
        .stdout(out.try_clone().map_err(|e| err(np, e.to_string()))?)
        .stderr(out)
        .spawn()
        .map_err(|e| err(np, format!("cannot run the adapter's {what}: {e}")))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.success()),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err(
                    np,
                    format!(
                        "the adapter's {what} did not finish in {} s (log: {})",
                        timeout.as_secs(),
                        log.display()
                    ),
                ));
            }
            Err(e) => return Err(err(np, format!("waiting for the adapter's {what}: {e}"))),
        }
    }
}

impl ZoneBackend for AttachBackend {
    fn name(&self) -> &'static str {
        "attach"
    }

    fn up(&self, _sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        let (dir, adapter) = load(np)?;
        if run(
            np,
            &dir,
            runtime_dir,
            "status",
            &adapter.status,
            STATUS_TIMEOUT,
        )? {
            return Ok(()); // idempotent: the platform is already up
        }
        let timeout = Duration::from_secs(adapter.start_timeout_s);
        if !run(np, &dir, runtime_dir, "start", &adapter.start, timeout)? {
            return Err(err(
                np,
                "the adapter's start failed; see the node's attach log",
            ));
        }
        if !run(
            np,
            &dir,
            runtime_dir,
            "status",
            &adapter.status,
            STATUS_TIMEOUT,
        )? {
            return Err(err(
                np,
                "the adapter's start returned, but its status does not report the node up",
            ));
        }
        Ok(())
    }

    fn down(&self, _sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
        let (dir, adapter) = load(np)?;
        if !run(
            np,
            &dir,
            runtime_dir,
            "status",
            &adapter.status,
            STATUS_TIMEOUT,
        )? {
            return Ok(()); // idempotent: nothing to stop
        }
        if !run(np, &dir, runtime_dir, "stop", &adapter.stop, STOP_TIMEOUT)? {
            return Err(err(
                np,
                "the adapter's stop failed; see the node's attach log",
            ));
        }
        Ok(())
    }

    fn status(&self, _sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> NodeState {
        let Ok((dir, adapter)) = load(np) else {
            return NodeState::Down;
        };
        match run(
            np,
            &dir,
            runtime_dir,
            "status",
            &adapter.status,
            STATUS_TIMEOUT,
        ) {
            Ok(true) => NodeState::Up,
            _ => NodeState::Down,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(dir: &Path) -> NodePlan {
        NodePlan {
            zone_id: "cockpit".into(),
            node_id: format!("ivi-test-{}", std::process::id()),
            role: crate::model::DeviceRole::Ecu,
            backend: crate::model::Backend::Attach,
            endpoint: "tap-ivi".into(),
            peer: "tap-ivi".into(),
            kind: crate::gateway::plan::EndpointKind::Adopted,
            vlan: 101,
            slice: Some("ivi".into()),
            ip_cidr: Some("192.168.98.3/24".into()),
            extra_cidrs: Vec::new(),
            bundle: Some(dir.to_path_buf()),
            module: None,
            remote_bundle: None,
            image: None,
            pinned_cpus: Vec::new(),
        }
    }

    fn sh() -> Shell {
        Shell {
            netns: None,
            escalate: false,
        }
    }

    /// A platform simulated by a marker file: start creates it, stop removes it, status reads it.
    fn adapter(dir: &Path) {
        std::fs::write(
            dir.join("attach.yaml"),
            "interface: tap-ivi\nstart: touch up && echo \"$VX_NODE $VX_IP\" > started\n\
             status: test -f up\nstop: rm up\nstart_timeout_s: 5\n",
        )
        .unwrap();
    }

    #[test]
    fn up_starts_once_and_down_stops_once() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tempfile::tempdir().unwrap();
        adapter(dir.path());
        let np = node(dir.path());
        let b = AttachBackend;
        assert_eq!(b.status(&sh(), &np, rt.path()), NodeState::Down);
        b.up(&sh(), &np, rt.path()).unwrap();
        assert_eq!(b.status(&sh(), &np, rt.path()), NodeState::Up);
        let started = std::fs::read_to_string(dir.path().join("started")).unwrap();
        assert_eq!(started.trim(), format!("{} 192.168.98.3/24", np.node_id));
        // A second up finds the platform up and does not start it again.
        std::fs::remove_file(dir.path().join("started")).unwrap();
        b.up(&sh(), &np, rt.path()).unwrap();
        assert!(!dir.path().join("started").exists());
        b.down(&sh(), &np, rt.path()).unwrap();
        assert_eq!(b.status(&sh(), &np, rt.path()), NodeState::Down);
        b.down(&sh(), &np, rt.path()).unwrap(); // idempotent
    }

    #[test]
    fn an_adapter_for_another_interface_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tempfile::tempdir().unwrap();
        adapter(dir.path());
        let mut np = node(dir.path());
        np.endpoint = "other0".into();
        let e = AttachBackend
            .up(&sh(), &np, rt.path())
            .unwrap_err()
            .to_string();
        assert!(e.contains("not the one the link endpoint adopts"), "{e}");
    }

    #[test]
    fn a_start_that_never_reports_up_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("attach.yaml"),
            "interface: tap-ivi\nstart: \"true\"\nstatus: \"false\"\nstop: \"true\"\n",
        )
        .unwrap();
        let e = AttachBackend
            .up(&sh(), &node(dir.path()), rt.path())
            .unwrap_err()
            .to_string();
        assert!(e.contains("does not report the node up"), "{e}");
    }
}
