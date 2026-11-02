//! `wasm-capsule` zone backend (Track V5): a WebAssembly module as a zone node, for a sensor or
//! ECU simulable as a sandboxed control loop rather than a full guest kernel.
//!
//! The node's network is a container node's: a named namespace holding the gateway port as
//! `eth0`. Its payload is a `vxcapsule` process inside that namespace, so `wasmtime` never enters
//! `vxedged`. See the `vxcapsule` crate for why it is a process, and for the capsule contract.
//!
//! The runner starts from a thread that has entered the namespace, because a child inherits its
//! thread's. Exec drops `vxedged`'s file capabilities, so it runs unprivileged as the same user.
//! `setsid -f` detaches it, since it must outlive the caller and not linger as a zombie.
//!
//! **Liveness comes from `/proc`, not a pidfile.** The runner's command line names its node, so
//! any daemon finds it whatever its runtime dir.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::backend::{ZoneBackendError, container_down, netns_port_up, wait_until};
use super::foreign;
use super::plan::NodePlan;
use crate::gateway::{Shell, netlink};
use crate::zones::backend::NodeState;

const RUNNER: &str = "vxcapsule";
/// Written by the runner once the guest is instantiated.
const READY_LINE: &str = "vxcapsule: running";
/// How long a capsule gets to compile and start. Compiling is the slow part.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a signalled runner gets to exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

fn err(np: &NodePlan, detail: String) -> ZoneBackendError {
    ZoneBackendError::Runtime {
        node: np.node_id.clone(),
        detail,
    }
}

fn log_path(np: &NodePlan, runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(format!("{}.capsule.log", np.runtime_name()))
}

/// The runner's pid, if one is running this node.
fn running_pid(np: &NodePlan) -> Option<i32> {
    foreign::guests()
        .find(|(_, node)| node == &np.node_id)
        .map(|(pid, _)| pid)
}

/// The runner: `VXEDGE_CAPSULE_BIN`, else beside this executable, else on PATH. Absolute, since
/// it is an install location rather than a name to search for.
fn runner_path() -> Option<PathBuf> {
    let explicit = std::env::var_os("VXEDGE_CAPSULE_BIN").map(PathBuf::from);
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(RUNNER)));
    let on_path = std::env::var_os("PATH").into_iter().flat_map(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(RUNNER))
            .collect::<Vec<_>>()
    });
    explicit
        .into_iter()
        .chain(beside)
        .chain(on_path)
        .find(|p| p.is_file())
}

/// The module in a pulled part. A part holds exactly one, beside its `meta.json`.
pub(super) fn module_in(np: &NodePlan, dir: &Path) -> Result<PathBuf, ZoneBackendError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        err(
            np,
            format!("cannot read pulled part `{}`: {e}", dir.display()),
        )
    })?;
    let modules: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "wasm"))
        .collect();
    match modules.as_slice() {
        [module] => Ok(module.clone()),
        other => Err(err(
            np,
            format!(
                "pulled part `{}` holds {} .wasm files, and a capsule part holds one. Republish \
                 it, or correct the part's `ref`.",
                dir.display(),
                other.len()
            ),
        )),
    }
}

fn launch_argv(runner: &Path, np: &NodePlan, module: &Path) -> Vec<String> {
    let mut argv: Vec<String> = ["setsid", "-f"].map(String::from).to_vec();
    if !np.pinned_cpus.is_empty() {
        let cpus: Vec<String> = np.pinned_cpus.iter().map(u32::to_string).collect();
        argv.extend(["taskset".to_string(), "-c".to_string(), cpus.join(",")]);
    }
    argv.push(runner.display().to_string());
    argv.push("--node".to_string());
    argv.push(np.node_id.clone());
    argv.push(module.display().to_string());
    argv
}

fn launch(np: &NodePlan, argv: &[String], log: &Path) -> Result<(), ZoneBackendError> {
    let open_log = |truncate: bool| {
        std::fs::File::options()
            .create(true)
            .write(true)
            .truncate(truncate)
            .append(!truncate)
            .open(log)
            .map_err(|e| {
                err(
                    np,
                    format!(
                        "cannot open `{}` for the capsule's log: {e}. Check `--runtime-dir` is \
                         writable by this user.",
                        log.display()
                    ),
                )
            })
    };
    // One log per start. Reopened for append so both streams keep their place in it.
    drop(open_log(true)?);
    let (stdout, stderr) = (open_log(false)?, open_log(false)?);
    let ns = np.runtime_name();
    let argv = argv.to_vec();
    let status = netlink::in_netns(&ns, move || {
        Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .status()
    })
    .map_err(|e| {
        err(
            np,
            format!(
                "cannot enter namespace `{}` to start the capsule: {e}. `vxedged` needs \
                 `cap_sys_admin` for this (`sudo setcap cap_net_admin,cap_sys_admin+ep vxedged`).",
                np.runtime_name()
            ),
        )
    })?
    .map_err(|e| err(np, format!("cannot run `setsid`: {e}. Install util-linux.")))?;
    if !status.success() {
        return Err(err(
            np,
            format!(
                "`setsid` exited with {status} before `{RUNNER}` started. See `{}`.",
                log.display()
            ),
        ));
    }
    Ok(())
}

fn log_has_ready_line(log: &Path) -> bool {
    std::fs::read_to_string(log).is_ok_and(|text| text.lines().any(|l| l == READY_LINE))
}

fn log_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(5)..].join("\n")
}

pub(super) fn up(sh: &Shell, np: &NodePlan, runtime_dir: &Path) -> Result<(), ZoneBackendError> {
    if running_pid(np).is_some() {
        return Ok(()); // idempotent
    }
    let module = np
        .module
        .as_deref()
        .expect("plan invariant: a wasm-capsule node has a module, or node_up fetched it");
    if !module.is_file() {
        return Err(err(
            np,
            format!(
                "capsule module `{}` does not exist. Build it, or correct the part's `ref`.",
                module.display()
            ),
        ));
    }
    let Some(runner) = runner_path() else {
        return Err(err(
            np,
            format!(
                "`{RUNNER}` was not found beside this binary or on PATH. It ships with vxedge; \
                 install it next to `vxedged`, or set VXEDGE_CAPSULE_BIN."
            ),
        ));
    };
    netns_port_up(sh, np)?;

    let log = log_path(np, runtime_dir);
    launch(np, &launch_argv(&runner, np, module), &log)?;
    let mut exited = false;
    let ready = wait_until(START_TIMEOUT, || {
        let ready = log_has_ready_line(&log);
        exited = !ready && running_pid(np).is_none() && !log_tail(&log).is_empty();
        ready || exited
    });
    if ready && !exited {
        return Ok(());
    }
    stop(np)?;
    Err(err(
        np,
        format!(
            "the capsule did not start. Its log, `{}`, ends:\n{}",
            log.display(),
            log_tail(&log)
        ),
    ))
}

/// Ends the runner. SIGKILL, because a capsule is stateless and signals are the only stop an idle
/// one has, and the kernel reaps it whatever it was blocked on.
fn stop(np: &NodePlan) -> Result<(), ZoneBackendError> {
    let Some(pid) = running_pid(np) else {
        return Ok(());
    };
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    if wait_until(STOP_TIMEOUT, || running_pid(np).is_none()) {
        return Ok(());
    }
    Err(err(
        np,
        format!(
            "`{RUNNER}` (pid {pid}) did not exit after SIGKILL. If another user started it, stop \
             it as that user. Otherwise check `ps -o stat,cmd -p {pid}`."
        ),
    ))
}

pub(super) fn down(sh: &Shell, np: &NodePlan) -> Result<(), ZoneBackendError> {
    stop(np)?;
    container_down(sh, np)
}

pub(super) fn status(np: &NodePlan) -> NodeState {
    match running_pid(np) {
        Some(_) => NodeState::Up,
        None => NodeState::Down,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::plan::EndpointKind;
    use crate::model::{Backend, DeviceRole};

    fn node(pinned_cpus: Vec<u32>) -> NodePlan {
        NodePlan {
            zone_id: "z".into(),
            node_id: "sensor-01".into(),
            role: DeviceRole::Sensor,
            backend: Backend::WasmCapsule,
            endpoint: "vz-sensor-01".into(),
            peer: "vzp-sensor-01".into(),
            kind: EndpointKind::Veth,
            vlan: 101,
            slice: Some("adas".into()),
            ip_cidr: Some("10.101.0.2/24".into()),
            bundle: None,
            remote_bundle: None,
            image: None,
            module: Some("/parts/sensor.wasm".into()),
            pinned_cpus,
        }
    }

    #[test]
    fn the_runner_starts_detached_and_named_after_its_node() {
        assert_eq!(
            launch_argv(
                Path::new("/opt/vxcapsule"),
                &node(vec![]),
                Path::new("/m.wasm")
            ),
            [
                "setsid",
                "-f",
                "/opt/vxcapsule",
                "--node",
                "sensor-01",
                "/m.wasm"
            ]
        );
    }

    #[test]
    fn a_pinned_node_keeps_the_whole_runner_on_its_cpus() {
        let argv = launch_argv(Path::new("/r"), &node(vec![2, 3]), Path::new("/m"));
        assert_eq!(argv[2..6], ["taskset", "-c", "2,3", "/r"]);
    }

    #[test]
    fn a_pulled_part_must_hold_exactly_one_module() {
        let dir = std::env::temp_dir().join(format!("vx-capsule-part-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), b"{}").unwrap();
        assert!(module_in(&node(vec![]), &dir).is_err());
        std::fs::write(dir.join("a.wasm"), b"").unwrap();
        assert_eq!(module_in(&node(vec![]), &dir).unwrap(), dir.join("a.wasm"));
        std::fs::write(dir.join("b.wasm"), b"").unwrap();
        let e = module_in(&node(vec![]), &dir).unwrap_err().to_string();
        assert!(e.contains("holds 2 .wasm files"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_node_nothing_runs_is_down() {
        assert_eq!(status(&node(vec![])), NodeState::Down);
    }

    #[test]
    fn the_ready_line_must_be_a_whole_line() {
        let dir = std::env::temp_dir().join(format!("vx-capsule-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("n.log");
        std::fs::write(&log, "guest says vxcapsule: running\n").unwrap();
        assert!(!log_has_ready_line(&log));
        std::fs::write(&log, "vxcapsule: running\n").unwrap();
        assert!(log_has_ready_line(&log));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
