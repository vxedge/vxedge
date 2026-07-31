//! Generic service lifecycle (`ota`, `telemetry` — §8), dispatched by `SvcService`. v1 spawns
//! the service binary as a host-local child process co-located with `vxedged`, pidfile-tracked
//! the same way the qemu-microvm zone backend tracks its guest — not yet scheduled onto a
//! `vxcompute` node, since that backend doesn't exist yet (§8, §10).
//!
//! One seam, [`ServiceBackend`], and it is deliberately narrow: a backend translates a
//! `ServiceDef` into a command line and nothing else. Process supervision — idempotent `up`,
//! the pidfile, reaping the child, graceful-then-forceful `down`, liveness — is identical for
//! every service and lives here once. That split is not cosmetic: the zombie-process bug a live
//! smoke test found lives entirely in the shared half, and a per-backend copy of it would have
//! meant finding it again per service.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One entry of an experiment's `services:` block: what to run, and how it is packaged.
///
/// Lives here rather than in a schema module because this is the only thing that reads it. The
/// domain model owns the `services:` block — a service is a runtime descriptor rather than part
/// of the vehicle under test, so it sits on the experiment beside `traffic` and `telemetry`,
/// not on any entity. The type lives here because this module is what runs one.
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
    /// Every service type this experiment declares — what `experiment up` fans out over.
    pub fn declared_types(exp: &crate::model::Experiment) -> Vec<String> {
        exp.services
            .values()
            .map(|d| d.service_type.as_str().to_string())
            .collect()
    }

    /// The service of `service_type` this experiment declares, if any.
    ///
    /// No parsing: the model typed `services:` at build time, so a malformed block was already
    /// reported against the document the author wrote. This used to deserialize raw YAML on
    /// every call, which meant a typo'd `type:` surfaced at `svc up` — once per service — with
    /// the file long out of sight.
    pub fn find<'a>(exp: &'a crate::model::Experiment, service_type: &str) -> Option<&'a Self> {
        exp.services
            .values()
            .find(|d| d.service_type.as_str() == service_type)
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

/// Telemetry config, all optional: the scrape targets live in the workspace's own
/// `telemetry.prometheus` block, so this only says how to sample them.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TelemetryConfig {
    #[serde(default)]
    ingest_dir: Option<String>,
    #[serde(default = "default_interval_s")]
    interval_s: u64,
    #[serde(default = "default_duration_s")]
    duration_s: u64,
}

fn default_interval_s() -> u64 {
    5
}

fn default_duration_s() -> u64 {
    60
}

/// Where the binaries a service backend spawns actually live.
///
/// A struct rather than a positional `&str` per service: v1 co-locates every service with
/// `vxedged` on the host (`ARCHITECTURE.md` §8), so each new service type adds a field here
/// instead of a parameter to every caller. When `vxcompute` scheduling lands, this is the one
/// place that stops being "a path on this host".
#[derive(Debug, Clone)]
pub struct ServiceBinaries {
    pub vxota: String,
    pub vxtelemetry: String,
}

/// What to run for one service, resolved from its manifest `config` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub bin: String,
    pub args: Vec<String>,
    /// Human-readable summary for the job result — what actually got run.
    pub summary: String,
}

/// One service type's translation from manifest to command line.
///
/// Deliberately *only* that. Process supervision — the idempotence check, the pidfile, reaping
/// the child, graceful-then-forceful teardown, liveness — is identical for every service and
/// lives once in this module. A backend that re-implemented any of it would be re-implementing
/// the zombie-process fix that a live smoke test found (see [`svc_up`]), which is exactly the
/// class of bug that hides in a per-backend copy.
///
/// Contract for an implementor: `command` is **pure**. It reads `def.config`, validates it, and
/// returns what to run — no spawning, no filesystem writes, no side effects. That keeps a new
/// service type unit-testable without a process, and keeps every service's failure mode the
/// same shape.
pub trait ServiceBackend {
    fn service_type(&self) -> ServiceType;

    fn command(
        &self,
        def: &ServiceDef,
        bins: &ServiceBinaries,
        workspace_dir: &Path,
        runtime_dir: &Path,
    ) -> Result<SpawnSpec, ServiceBackendError>;
}

struct OtaBackend;
struct TelemetryBackend;

impl ServiceBackend for OtaBackend {
    fn service_type(&self) -> ServiceType {
        ServiceType::Ota
    }

    fn command(
        &self,
        def: &ServiceDef,
        bins: &ServiceBinaries,
        _workspace_dir: &Path,
        _runtime_dir: &Path,
    ) -> Result<SpawnSpec, ServiceBackendError> {
        let cfg: OtaConfig = typed_config(def, "config.artifacts is required for an ota service")?;
        Ok(SpawnSpec {
            bin: bins.vxota.clone(),
            args: vec![
                "serve".into(),
                "--artifacts-dir".into(),
                cfg.artifacts.clone(),
                "--listen".into(),
                cfg.listen.clone(),
            ],
            summary: format!(
                "{} serve --artifacts-dir {} --listen {}",
                bins.vxota, cfg.artifacts, cfg.listen
            ),
        })
    }
}

impl ServiceBackend for TelemetryBackend {
    fn service_type(&self) -> ServiceType {
        ServiceType::Telemetry
    }

    fn command(
        &self,
        def: &ServiceDef,
        bins: &ServiceBinaries,
        workspace_dir: &Path,
        runtime_dir: &Path,
    ) -> Result<SpawnSpec, ServiceBackendError> {
        // Scrape targets are not repeated here: `vxtelemetry` reads them from the workspace's
        // own `telemetry.prometheus` block. Restating them in `services.telemetry.config` would
        // be a second place to edit that can disagree with the first.
        let cfg: TelemetryConfig =
            typed_config(def, "a telemetry service takes an optional config block")?;
        let ingest_dir = cfg
            .ingest_dir
            .unwrap_or_else(|| runtime_dir.join("ingest").to_string_lossy().to_string());
        Ok(SpawnSpec {
            bin: bins.vxtelemetry.clone(),
            args: vec![
                "ingest".into(),
                "--workspace".into(),
                workspace_dir.to_string_lossy().to_string(),
                "--ingest-dir".into(),
                ingest_dir.clone(),
                "--interval-s".into(),
                cfg.interval_s.to_string(),
                "--duration-s".into(),
                cfg.duration_s.to_string(),
            ],
            summary: format!(
                "{} ingest --workspace {} --ingest-dir {ingest_dir} ({}s every {}s)",
                bins.vxtelemetry,
                workspace_dir.display(),
                cfg.duration_s,
                cfg.interval_s
            ),
        })
    }
}

/// **The only place a service type is matched.** A new service type is one impl and one arm.
pub fn backend_for(service_type: ServiceType) -> &'static dyn ServiceBackend {
    match service_type {
        ServiceType::Ota => &OtaBackend,
        ServiceType::Telemetry => &TelemetryBackend,
    }
}

/// Deserialize a service's `config` block, defaulting an absent one so a service whose config
/// is entirely optional needs no special case in its backend.
fn typed_config<T: serde::de::DeserializeOwned>(
    def: &ServiceDef,
    missing: &str,
) -> Result<T, ServiceBackendError> {
    let value = def
        .config
        .clone()
        .unwrap_or(serde_yaml_ng::Value::Mapping(Default::default()));
    serde_yaml_ng::from_value(value).map_err(|e| ServiceBackendError::BadConfig {
        svc: def.id.clone(),
        detail: format!("{e} ({missing})"),
    })
}

/// Bring the service type declared by `def` up. Idempotent: a live process
/// for that type is left alone.
///
/// Everything type-specific is [`ServiceBackend::command`]; everything here is the supervision
/// every service shares.
pub fn svc_up(
    def: &ServiceDef,
    bins: &ServiceBinaries,
    workspace_dir: &Path,
    runtime_dir: &Path,
) -> Result<String, ServiceBackendError> {
    let backend = backend_for(def.service_type);
    let type_name = def.service_type.as_str();

    if svc_status(runtime_dir, type_name) == SvcState::Up {
        return Ok(format!("{type_name} service already up (idempotent)"));
    }

    let spec = backend.command(def, bins, workspace_dir, runtime_dir)?;

    let log_path = runtime_dir.join(format!("svc-{type_name}.log"));
    let log = std::fs::File::create(&log_path).map_err(|e| {
        ServiceBackendError::Runtime(
            def.id.clone(),
            format!("cannot open log {}: {e}", log_path.display()),
        )
    })?;
    let log_err = log
        .try_clone()
        .map_err(|e| ServiceBackendError::Runtime(def.id.clone(), e.to_string()))?;
    let mut child = Command::new(&spec.bin)
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|source| ServiceBackendError::Spawn {
            svc: def.id.clone(),
            bin: spec.bin.clone(),
            source,
        })?;
    let pid = child.id();
    std::fs::write(pidfile(runtime_dir, type_name), pid.to_string()).map_err(|e| {
        ServiceBackendError::Runtime(def.id.clone(), format!("cannot write pidfile: {e}"))
    })?;
    // vxedged is this child's OS-level parent regardless of whether we keep the `Child` handle;
    // without a `wait()` somewhere, a killed child becomes a zombie forever — /proc/<pid> and
    // `kill -0` both keep reporting it as "alive" even though it already exited, which makes
    // svc_down's liveness check spin its 300ms grace window and then wrongly conclude it needs
    // `kill -9` too, while the pid never actually goes away. Reap it as soon as it exits.
    // Found by a live smoke test, not by the unit tests — which is why this is shared
    // supervision rather than something each backend repeats.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(format!(
        "{type_name} service up — {} (pid {pid}, log {})",
        spec.summary,
        log_path.display()
    ))
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

    fn bins() -> ServiceBinaries {
        ServiceBinaries {
            vxota: "vxota".into(),
            vxtelemetry: "vxtelemetry".into(),
        }
    }

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
        let err = svc_up(&def, &bins(), tmp.path(), tmp.path()).unwrap_err();
        assert!(matches!(err, ServiceBackendError::BadConfig { .. }));
    }

    #[test]
    fn up_rejects_missing_config_entirely() {
        let tmp = tempfile::tempdir().unwrap();
        let mut def = ota_def("artifacts: /tmp/x");
        def.config = None;
        let err = svc_up(&def, &bins(), tmp.path(), tmp.path()).unwrap_err();
        assert!(matches!(err, ServiceBackendError::BadConfig { .. }));
    }

    #[test]
    fn up_reports_spawn_failure_for_missing_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let def = ota_def("artifacts: /tmp/x");
        let err = svc_up(
            &def,
            &ServiceBinaries {
                vxota: "/nonexistent/vxota-binary".into(),
                ..bins()
            },
            tmp.path(),
            tmp.path(),
        )
        .unwrap_err();
        assert!(matches!(err, ServiceBackendError::Spawn { .. }));
    }

    #[test]
    fn down_is_idempotent_with_no_pidfile() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(svc_down("ota", tmp.path()).is_ok());
    }

    /// Telemetry used to be `Unsupported`. It is a backend now, and the check that matters is
    /// that its command reads the *workspace* for scrape targets rather than restating them —
    /// a second copy of the target list is a second place to edit that can disagree.
    #[test]
    fn telemetry_renders_a_command_against_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let mut def = ota_def("interval_s: 2");
        def.service_type = ServiceType::Telemetry;
        let spec = backend_for(ServiceType::Telemetry)
            .command(
                &def,
                &bins(),
                std::path::Path::new("/ws/exp-001"),
                tmp.path(),
            )
            .expect("telemetry config is optional");
        assert_eq!(spec.bin, "vxtelemetry");
        assert!(
            spec.args
                .starts_with(&["ingest".to_string(), "--workspace".to_string()])
        );
        assert!(spec.args.contains(&"/ws/exp-001".to_string()));
        assert!(
            spec.args.contains(&"2".to_string()),
            "interval honoured: {:?}",
            spec.args
        );
        // Absent config is legitimate for telemetry — everything defaults.
        def.config = None;
        assert!(
            backend_for(ServiceType::Telemetry)
                .command(&def, &bins(), std::path::Path::new("/ws"), tmp.path())
                .is_ok()
        );
    }

    /// `command` is pure by contract: no spawn, no writes. That is what keeps a new service
    /// type unit-testable without a process on the host.
    #[test]
    fn a_backend_command_is_pure_and_selected_by_type() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            backend_for(ServiceType::Ota).service_type(),
            ServiceType::Ota
        );
        assert_eq!(
            backend_for(ServiceType::Telemetry).service_type(),
            ServiceType::Telemetry
        );
        let def = ota_def("artifacts: /tmp/x");
        let spec = backend_for(ServiceType::Ota)
            .command(&def, &bins(), tmp.path(), tmp.path())
            .unwrap();
        assert_eq!(spec.bin, "vxota");
        assert!(spec.args.contains(&"/tmp/x".to_string()));
        assert_eq!(
            std::fs::read_dir(tmp.path()).unwrap().count(),
            0,
            "command wrote something"
        );
    }
}
