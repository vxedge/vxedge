//! Generic service lifecycle (`ota`, `telemetry` — §8), dispatched by
//! `SvcService`. v1 spawns the service binary as a host-local child process
//! co-located with `vxedged`, pidfile-tracked the same way the qemu-microvm
//! zone backend tracks its guest — not yet scheduled onto a `vxcompute` node,
//! since that backend doesn't exist yet (§8, §10).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One entry of an experiment's `services:` block: what to run, and how it is packaged.
///
/// Lives here rather than in a schema module because this is the only thing that reads it. The
/// domain model carries `services` as untyped `passthrough` — a service is a runtime descriptor
/// rather than part of the vehicle under test, so typing it in the model would put a
/// deployment concern in the system model. [`ServiceDef::find`] is the one place raw YAML
/// becomes this type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDef {
    pub id: String,
    #[serde(rename = "type")]
    pub service_type: ServiceType,
    pub image: Image,
    #[serde(default)]
    pub config: Option<serde_yaml_ng::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceType {
    Ota,
    Telemetry,
}

impl ServiceType {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceType::Ota => "ota",
            ServiceType::Telemetry => "telemetry",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub url: String,
    pub kind: ImageKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageKind {
    Vm,
    Container,
    Rtimage,
}

impl ServiceDef {
    /// The service of `service_type` this experiment declares, if any.
    ///
    /// Reads the model's `passthrough["services"]`, which is the experiment document's own
    /// block verbatim. A malformed entry is an error rather than a miss: a typo'd `type:` that
    /// silently meant "no such service" would surface as "you declared no ota service" while
    /// one is sitting in the file.
    /// Every service type this experiment declares, in declaration order — what
    /// `experiment up` fans out over.
    pub fn declared_types(
        exp: &crate::model::Experiment,
    ) -> Result<Vec<String>, ServiceBackendError> {
        Ok(Self::all(exp)?
            .into_iter()
            .map(|d| d.service_type.as_str().to_string())
            .collect())
    }

    fn all(exp: &crate::model::Experiment) -> Result<Vec<Self>, ServiceBackendError> {
        let Some(raw) = exp.passthrough.get("services") else {
            return Ok(Vec::new());
        };
        let defs: BTreeMap<String, ServiceDef> =
            serde_yaml_ng::from_value(raw.clone()).map_err(|e| ServiceBackendError::BadConfig {
                svc: "services".to_string(),
                detail: e.to_string(),
            })?;
        Ok(defs.into_values().collect())
    }

    pub fn find(
        exp: &crate::model::Experiment,
        service_type: &str,
    ) -> Result<Option<Self>, ServiceBackendError> {
        Ok(Self::all(exp)?
            .into_iter()
            .find(|d| d.service_type.as_str() == service_type))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceBackendError {
    #[error("service `{0}`: this generic process backend only runs `ota` services so far")]
    Unsupported(String),
    #[error("service `{svc}`: bad config: {detail}")]
    BadConfig { svc: String, detail: String },
    #[error("service `{svc}`: cannot spawn `{bin}`: {source}")]
    Spawn {
        svc: String,
        bin: String,
        source: std::io::Error,
    },
    #[error("service `{0}`: {1}")]
    Runtime(String, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvcState {
    Up,
    Down,
}

fn pidfile(runtime_dir: &Path, service_type: &str) -> PathBuf {
    runtime_dir.join(format!("svc-{service_type}.pid"))
}

fn read_pid(runtime_dir: &Path, service_type: &str) -> Option<i32> {
    std::fs::read_to_string(pidfile(runtime_dir, service_type))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Observed state of the named service type (not a specific `ServiceDef` —
/// v1 runs at most one process per type, mirroring the RPC's `service_type`
/// string granularity).
pub fn svc_status(runtime_dir: &Path, service_type: &str) -> SvcState {
    match read_pid(runtime_dir, service_type) {
        Some(pid) if alive(pid) => SvcState::Up,
        _ => SvcState::Down,
    }
}

/// OTA config, as read from the manifest's `services.<key>.config` block
/// (ARCHITECTURE.md §16 worked example).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OtaConfig {
    artifacts: String,
    #[serde(default = "default_listen")]
    listen: String,
}

fn default_listen() -> String {
    "0.0.0.0:9900".to_string()
}

/// Bring the service type declared by `def` up. Idempotent: a live process
/// for that type is left alone.
pub fn svc_up(
    def: &ServiceDef,
    vxota_bin: &str,
    runtime_dir: &Path,
) -> Result<String, ServiceBackendError> {
    match def.service_type {
        ServiceType::Ota => {
            if svc_status(runtime_dir, "ota") == SvcState::Up {
                return Ok("ota service already up (idempotent)".to_string());
            }
            let cfg: OtaConfig = def
                .config
                .clone()
                .ok_or_else(|| ServiceBackendError::BadConfig {
                    svc: def.id.clone(),
                    detail: "config.artifacts is required for an ota service".to_string(),
                })
                .and_then(|v| {
                    serde_yaml_ng::from_value(v).map_err(|e| ServiceBackendError::BadConfig {
                        svc: def.id.clone(),
                        detail: e.to_string(),
                    })
                })?;

            let log_path = runtime_dir.join("svc-ota.log");
            let log = std::fs::File::create(&log_path).map_err(|e| {
                ServiceBackendError::Runtime(
                    def.id.clone(),
                    format!("cannot open log {}: {e}", log_path.display()),
                )
            })?;
            let log_err = log
                .try_clone()
                .map_err(|e| ServiceBackendError::Runtime(def.id.clone(), e.to_string()))?;
            let mut child = Command::new(vxota_bin)
                .args([
                    "serve",
                    "--artifacts-dir",
                    &cfg.artifacts,
                    "--listen",
                    &cfg.listen,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(log_err))
                .spawn()
                .map_err(|source| ServiceBackendError::Spawn {
                    svc: def.id.clone(),
                    bin: vxota_bin.to_string(),
                    source,
                })?;
            let pid = child.id();
            std::fs::write(pidfile(runtime_dir, "ota"), pid.to_string()).map_err(|e| {
                ServiceBackendError::Runtime(def.id.clone(), format!("cannot write pidfile: {e}"))
            })?;
            // vxedged is this child's OS-level parent regardless of whether we
            // keep the `Child` handle; without a `wait()` somewhere, a killed
            // child becomes a zombie forever — /proc/<pid> and `kill -0` both
            // keep reporting it as "alive" even though it already exited,
            // which makes svc_down's liveness check spin its 300ms grace
            // window and then wrongly conclude it needs `kill -9` too, while
            // the pid never actually goes away. Reap it as soon as it exits.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(format!(
                "ota service up — {vxota_bin} serve --artifacts-dir {} --listen {} (pid {pid}, log {})",
                cfg.artifacts,
                cfg.listen,
                log_path.display()
            ))
        }
        ServiceType::Telemetry => Err(ServiceBackendError::Unsupported(def.id.clone())),
    }
}

/// Tear the named service type down. Idempotent: no pidfile/dead pid is a no-op.
pub fn svc_down(service_type: &str, runtime_dir: &Path) -> Result<String, ServiceBackendError> {
    if let Some(pid) = read_pid(runtime_dir, service_type) {
        let _ = Command::new("kill").arg(pid.to_string()).output();
        std::thread::sleep(std::time::Duration::from_millis(300));
        if alive(pid) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
    }
    let _ = std::fs::remove_file(pidfile(runtime_dir, service_type));
    Ok(format!("{service_type} service down"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ota_def(config_yaml: &str) -> ServiceDef {
        ServiceDef {
            id: "ota-01".to_string(),
            service_type: ServiceType::Ota,
            image: Image {
                url: "unused".to_string(),
                kind: ImageKind::Container,
            },
            config: Some(serde_yaml_ng::from_str(config_yaml).unwrap()),
        }
    }

    #[test]
    fn status_is_down_with_no_pidfile() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(svc_status(tmp.path(), "ota"), SvcState::Down);
    }

    #[test]
    fn up_rejects_missing_artifacts_config() {
        let tmp = tempfile::tempdir().unwrap();
        let def = ota_def("listen: 0.0.0.0:9900");
        let err = svc_up(&def, "vxota", tmp.path()).unwrap_err();
        assert!(matches!(err, ServiceBackendError::BadConfig { .. }));
    }

    #[test]
    fn up_rejects_missing_config_entirely() {
        let tmp = tempfile::tempdir().unwrap();
        let mut def = ota_def("artifacts: /tmp/x");
        def.config = None;
        let err = svc_up(&def, "vxota", tmp.path()).unwrap_err();
        assert!(matches!(err, ServiceBackendError::BadConfig { .. }));
    }

    #[test]
    fn up_reports_spawn_failure_for_missing_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let def = ota_def("artifacts: /tmp/x");
        let err = svc_up(&def, "/nonexistent/vxota-binary", tmp.path()).unwrap_err();
        assert!(matches!(err, ServiceBackendError::Spawn { .. }));
    }

    #[test]
    fn down_is_idempotent_with_no_pidfile() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(svc_down("ota", tmp.path()).is_ok());
    }

    #[test]
    fn telemetry_type_is_unsupported_by_the_process_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let mut def = ota_def("artifacts: /tmp/x");
        def.service_type = ServiceType::Telemetry;
        let err = svc_up(&def, "vxota", tmp.path()).unwrap_err();
        assert!(matches!(err, ServiceBackendError::Unsupported(_)));
    }
}
