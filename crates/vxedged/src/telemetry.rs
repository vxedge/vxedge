//! `TelemetryService`: the results archive of a run, built by `vxtelemetry collect`.
//!
//! `Collect` archives what the workspace's telemetry service ingested, with the workspace itself,
//! the digests of every image it runs and the tool binaries the daemon runs (`tools.json`), and
//! returns the archive through `Job.artifact` with its sha256. `StreamRecords` and
//! `StreamMetrics` tail what the service is ingesting (`records`), and `LogService.Follow` the log
//! lines it reads.

use sha2::Digest;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use vxedge_api::v1::log_service_server::LogService;
use vxedge_api::v1::telemetry_service_server::TelemetryService;
use vxedge_api::v1::{
    CollectRequest, Job, LogFollowRequest, LogRecord, ManifestRef, Metric, Record,
    StreamMetricsRequest, StreamRecordsRequest, record,
};

use crate::audit::Audit;
use crate::errors::{self, R};
use crate::jobs::{JobEngine, JobFailure};
use crate::records;

pub struct TelemetrySvc {
    pub engine: JobEngine,
    pub audit: Arc<Audit>,
    pub runtime_dir: PathBuf,
    pub vxtelemetry_bin: String,
    /// The tool binaries this daemon runs, written to the ingest folder at every collect.
    pub tools: Arc<crate::tools::Tools>,
}

/// Every entity of `model`, with this daemon's exporter bound to `host` and the log files it
/// writes under `runtime_dir` bound to theirs.
pub(crate) fn entities(
    model: &vxedge_core::model::Experiment,
    runtime_dir: &Path,
) -> Vec<vxedge_core::entities::EntityRecord> {
    let mut entities = vxedge_core::entities::entities_with(model, &crate::exporter::targets());
    vxedge_core::entities::add_logs(&mut entities, model, runtime_dir);
    entities
}

/// The `entities.json` of `model`: [`entities`], as the telemetry service reads it.
pub(crate) fn entities_json(model: &vxedge_core::model::Experiment, runtime_dir: &Path) -> Vec<u8> {
    serde_json::to_vec_pretty(&entities(model, runtime_dir)).expect("entities always serialize")
}

/// `LogService`: the log lines the workspace's telemetry service reads, live.
pub struct LogSvc {
    pub runtime_dir: PathBuf,
}

#[tonic::async_trait]
impl LogService for LogSvc {
    type FollowStream = Pin<Box<dyn Stream<Item = Result<LogRecord, Status>> + Send>>;

    async fn follow(
        &self,
        req: Request<LogFollowRequest>,
    ) -> Result<Response<Self::FollowStream>, Status> {
        let req = req.into_inner();
        let ingest = ingest_of(&self.runtime_dir, req.experiment.as_ref()).await?;
        let (prefix, streams) = (req.entity_prefix, req.streams);
        let stream = records::stream(
            records::Tail {
                path: ingest.join(records::LOGS),
                after: req.after.max(0),
                follow: req.follow,
            },
            records::parse_log,
            move |l| {
                records::under(&l.entity, &prefix)
                    && (streams.is_empty() || streams.contains(&l.stream))
            },
            |l| l,
        );
        Ok(Response::new(Box::pin(stream)))
    }
}

fn sha256_of(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(crate::services::hex::encode(sha2::Sha256::digest(&bytes)))
}

impl TelemetrySvc {
    async fn ingest_of(&self, experiment: Option<&ManifestRef>) -> Result<PathBuf, Status> {
        ingest_of(&self.runtime_dir, experiment).await
    }
}

/// The ingest folder of the workspace `experiment` names, else of the live run's workspace, else
/// this daemon's default one.
async fn ingest_of(
    runtime_dir: &Path,
    experiment: Option<&ManifestRef>,
) -> Result<PathBuf, Status> {
    let live = || {
        crate::services::active_run(runtime_dir).map(|r| ManifestRef {
            source: Some(vxedge_api::v1::manifest_ref::Source::Path(
                r.dir.to_string_lossy().into_owned(),
            )),
            ..Default::default()
        })
    };
    let Some(mref) = experiment
        .filter(|m| m.source.is_some())
        .cloned()
        .or_else(live)
    else {
        return Ok(runtime_dir.join("ingest"));
    };
    let (yaml, _, _) = crate::services::resolve_manifest(Some(&mref)).await?;
    let model = vxedge_core::model::load_workspace_yaml(&yaml)
        .map_err(|e| errors::invalid(R::WorkspaceInvalid, e.to_string()))?;
    match vxedge_core::services::ServiceDef::find(&model, "telemetry") {
        Some(def) => vxedge_core::services::telemetry_ingest_dir(def, runtime_dir)
            .map_err(|e| errors::invalid(R::WorkspaceInvalid, e.to_string())),
        None => Ok(runtime_dir.join("ingest")),
    }
}

#[tonic::async_trait]
impl TelemetryService for TelemetrySvc {
    type StreamMetricsStream = Pin<Box<dyn Stream<Item = Result<Metric, Status>> + Send>>;

    type StreamRecordsStream = Pin<Box<dyn Stream<Item = Result<Record, Status>> + Send>>;

    async fn stream_records(
        &self,
        req: Request<StreamRecordsRequest>,
    ) -> Result<Response<Self::StreamRecordsStream>, Status> {
        let req = req.into_inner();
        let ingest = self.ingest_of(req.experiment.as_ref()).await?;
        let prefix = req.entity_prefix;
        let stream = records::stream(
            records::Tail {
                path: ingest.join(records::FILE),
                after: req.after.max(0),
                follow: req.follow,
            },
            records::parse,
            move |r| records::entity(r).is_none_or(|e| records::under(e, &prefix)),
            |r| r,
        );
        Ok(Response::new(Box::pin(stream)))
    }

    async fn stream_metrics(
        &self,
        req: Request<StreamMetricsRequest>,
    ) -> Result<Response<Self::StreamMetricsStream>, Status> {
        let req = req.into_inner();
        let ingest = self.ingest_of(req.experiment.as_ref()).await?;
        let selector: std::collections::BTreeMap<String, String> =
            req.selector.into_iter().collect();
        let stream = records::stream(
            records::Tail {
                path: ingest.join(records::FILE),
                after: 0,
                follow: true,
            },
            records::parse,
            move |r| matches!(&r.record, Some(record::Record::Sample(s)) if records::selected(s, &selector)),
            |r| match r.record {
                Some(record::Record::Sample(s)) => Metric {
                    name: s.metric,
                    labels: s.labels,
                    value: s.value,
                    ts_unix_ms: r.ts_ns / 1_000_000,
                    entity: s.entity,
                    ts_ns: r.ts_ns,
                },
                _ => unreachable!("only samples are kept"),
            },
        );
        Ok(Response::new(Box::pin(stream)))
    }

    async fn collect(&self, req: Request<CollectRequest>) -> Result<Response<Job>, Status> {
        // `archive_version` is ignored: vxtelemetry writes `vxarchive/2`, the one format there is.
        let req = req.into_inner();
        let (yaml, sha256, dir) = crate::services::resolve_manifest(req.manifest.as_ref()).await?;
        let workspace = dir.ok_or_else(|| {
            errors::invalid(
                R::InvalidRequest,
                "collect needs a workspace directory: pass a path, not an inline manifest",
            )
        })?;
        let model = vxedge_core::model::load_workspace_yaml(&yaml)
            .map_err(|e| errors::invalid(R::WorkspaceInvalid, e.to_string()))?;
        let ingest = match vxedge_core::services::ServiceDef::find(&model, "telemetry") {
            Some(def) => vxedge_core::services::telemetry_ingest_dir(def, &self.runtime_dir)
                .map_err(|e| errors::invalid(R::WorkspaceInvalid, e.to_string()))?,
            None => self.runtime_dir.join("ingest"),
        };
        if !ingest.is_dir() {
            return Err(errors::precondition(
                R::NotFound,
                format!(
                    "nothing has been ingested: `{}` does not exist. Run the workspace's \
                     telemetry service first",
                    ingest.display()
                ),
            ));
        }
        // The entity paths every record names, mapped once here from the model (§12), so the
        // archive carries them beside the samples that use them.
        let entities_file = ingest.join("entities.json");
        std::fs::write(&entities_file, entities_json(&model, &self.runtime_dir)).map_err(|e| {
            errors::invalid(
                R::Unspecified,
                format!("cannot write `{}`: {e}", entities_file.display()),
            )
        })?;
        // The binaries this daemon runs, which vxtelemetry records as `metadata.json`'s `tools`.
        self.tools.write(&ingest).await.map_err(|e| {
            errors::invalid(
                R::Unspecified,
                format!(
                    "cannot write `{}`: {e}",
                    ingest.join(crate::tools::FILE).display()
                ),
            )
        })?;
        let out = if req.out_path.is_empty() {
            workspace
                .join("results")
                .join(format!("{}-{}.zip", model.id, crate::jobs::now_ms()))
        } else {
            let p = PathBuf::from(&req.out_path);
            if p.is_absolute() {
                p
            } else {
                workspace.join(p)
            }
        };
        let mut images: Vec<String> = model.placements().map(|p| p.image.url.clone()).collect();
        images.extend(model.entities.iter().flat_map(|e| {
            e.functions
                .values()
                .filter_map(|f| f.part.as_ref())
                .filter(|p| p.kind == vxedge_core::model::PartKind::Container)
                .map(|p| p.reference.clone())
        }));
        images.extend(
            vxedge_core::fiveg::fiveg_images(&model)
                .into_iter()
                .chain(vxedge_core::gptp::gptp_images(&model))
                .map(str::to_string),
        );
        images.sort();
        images.dedup();
        // Whether the run being archived checked its lock is the daemon's to say: vxtelemetry
        // records what it is told and nothing when told nothing. Only the live run of this same
        // workspace counts. Passed in the environment, which an older vxtelemetry ignores.
        let same = |a: &Path, b: &Path| a.canonicalize().ok() == b.canonicalize().ok();
        let run_package = crate::services::active_run(&self.runtime_dir)
            .filter(|r| same(&r.dir, &workspace))
            .and_then(|r| r.package);
        let mut audit = serde_json::json!({
            "workspace_sha256": sha256, "out": out,
            "workspace_identity_algorithm": crate::services::identity_algorithm(Some(&workspace)),
        });
        if let (Some(fields), Some(p)) = (audit.as_object_mut(), &run_package) {
            fields.insert("package_digest".into(), p.digest.clone().into());
            fields.insert("package_locked".into(), p.locked.into());
        }
        self.audit.record("TelemetryService.Collect", audit);
        let package_locked = run_package.map(|p| p.locked);
        let bin = self.vxtelemetry_bin.clone();
        let log = self.runtime_dir.join("collect.log");
        let job = self
            .engine
            .submit("telemetry.collect", move |p| async move {
                p.update(10, &format!("archiving {}", ingest.display()));
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("cannot create `{}`: {e}", parent.display()))?;
                }
                let mut cmd = tokio::process::Command::new(&bin);
                cmd.arg("collect")
                    .arg("--workspace")
                    .arg(&workspace)
                    .arg("--ingest-dir")
                    .arg(&ingest)
                    .arg("--out")
                    .arg(&out);
                for image in &images {
                    cmd.arg("--record-image").arg(image);
                }
                if let Some(locked) = package_locked {
                    cmd.env("VXTELEMETRY_PACKAGE_LOCKED", locked.to_string());
                }
                let output = cmd
                    .output()
                    .await
                    .map_err(|e| format!("cannot run `{bin}`: {e}"))?;
                let _ = std::fs::write(&log, [&output.stdout[..], &output.stderr[..]].concat());
                if !output.status.success() {
                    return Err(JobFailure::from(format!(
                        "`{bin} collect` failed ({}): see {}",
                        output.status,
                        log.display()
                    )));
                }
                let sha = sha256_of(&out)
                    .map_err(|e| format!("no archive at `{}`: {e}", out.display()))?;
                p.artifact(&out, &sha);
                Ok::<_, JobFailure>(format!("archive {} (sha256 {sha})", out.display()))
            })
            .await;
        Ok(Response::new(job))
    }
}
