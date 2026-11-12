//! The campaign loop (`CampaignService.Start`): each trial of a campaign's plan, serially, as
//! stages of one `campaign.run` job (decisions X-22, X-23).
//!
//! A trial is: provision (`experiment up` with the trial's bound document), warmup (traffic up,
//! then wait), measure (measurements for the trial's duration), cooldown (measurements and
//! traffic stop), teardown (the telemetry service stops, the trial is archived, `experiment
//! down`). Each step is an in-process call to the service that owns it, made as a stage of the
//! campaign job, so every check those services make applies and every step shows as a child job.
//! A phase marker goes to the ingest folder's `phases.jsonl` as each phase starts, and vxtelemetry
//! moves it into the trial's records.
//!
//! The trial's document is bound in memory and sent inline with its workspace folder: the package
//! is never written. A failed trial is recorded and the next one runs; `experiment down` runs after
//! every trial that got as far as provisioning. The campaign's state lives in `campaign.json` beside
//! its archives and under the runtime folder's `campaigns/`, so `Get`, `List` and a resumed `Start`
//! read it after a restart.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

use vxedge_api::v1::experiment_service_server::ExperimentService;
use vxedge_api::v1::manifest_ref::Source;
use vxedge_api::v1::measurement_service_server::MeasurementService;
use vxedge_api::v1::svc_service_server::SvcService;
use vxedge_api::v1::telemetry_service_server::TelemetryService;
use vxedge_api::v1::traffic_service_server::TrafficService;
use vxedge_api::v1::{
    CampaignState, CampaignStatus, CampaignTrialStatus, CollectRequest, ExperimentDownRequest,
    ExperimentUpRequest, ManifestRef, MeasurementStartRequest, MeasurementStatusRequest,
    MeasurementStopRequest, SvcDownRequest, TrafficDownRequest, TrafficUpRequest, TrialState,
};
use vxedge_core::campaign::Trial;

use crate::experiment::{ExperimentSvc, await_job};
use crate::jobs::{JobEngine, JobFailure, Progress, now_ms, stage};
use crate::measure::MeasurementSvc;
use crate::services::{SvcSvc, TrafficSvc};
use crate::telemetry::TelemetrySvc;

/// The services a trial's steps call.
#[derive(Clone)]
pub struct Steps {
    pub engine: JobEngine,
    pub experiments: Arc<ExperimentSvc>,
    pub traffic: Arc<TrafficSvc>,
    pub measure: Arc<MeasurementSvc>,
    pub telemetry: Arc<TelemetrySvc>,
    pub svcs: Arc<SvcSvc>,
}

/// One trial in `campaign.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrialRecord {
    pub id: String,
    #[serde(default)]
    pub arm: String,
    pub repetition: u32,
    #[serde(default)]
    pub parameters: serde_json::Value,
    pub state: String,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub archive: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub error: String,
}

/// A campaign, as `campaign.json` holds it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CampaignRecord {
    pub campaign_id: String,
    // A record published with a frozen campaign's archives names its trials and no more.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub job_id: String,
    #[serde(default)]
    pub experiment_id: String,
    #[serde(default)]
    pub package_digest: String,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub caller: String,
    #[serde(default)]
    pub unlocked: bool,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub results_dir: String,
    /// The workspace whose `results/` records the experiment's reference campaigns, when the
    /// start named one (`reference_workspace`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reference_workspace: String,
    pub trials: Vec<TrialRecord>,
    #[serde(default)]
    pub started_unix_ms: i64,
    #[serde(default)]
    pub finished_unix_ms: i64,
    /// `pass`, `fail` or `inconclusive`, judged when the campaign finished; empty before, and for
    /// an experiment without criteria.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verdict: String,
}

fn trial_state(s: &str) -> TrialState {
    match s {
        "pending" => TrialState::Pending,
        "running" => TrialState::Running,
        "succeeded" => TrialState::Succeeded,
        "failed" => TrialState::Failed,
        "skipped" => TrialState::Skipped,
        _ => TrialState::Unspecified,
    }
}

fn campaign_state(s: &str) -> CampaignState {
    match s {
        "running" => CampaignState::Running,
        "succeeded" => CampaignState::Succeeded,
        "failed" => CampaignState::Failed,
        "cancelled" => CampaignState::Cancelled,
        _ => CampaignState::Unspecified,
    }
}

impl CampaignRecord {
    pub fn status(&self) -> CampaignStatus {
        CampaignStatus {
            campaign_id: self.campaign_id.clone(),
            state: campaign_state(&self.state) as i32,
            job_id: self.job_id.clone(),
            experiment_id: self.experiment_id.clone(),
            package_digest: self.package_digest.clone(),
            profile: self.profile.clone(),
            caller: self.caller.clone(),
            unlocked: self.unlocked,
            results_dir: self.results_dir.clone(),
            trials: self
                .trials
                .iter()
                .map(|t| CampaignTrialStatus {
                    id: t.id.clone(),
                    arm: t.arm.clone(),
                    repetition: t.repetition,
                    state: trial_state(&t.state) as i32,
                    phase: t.phase.clone(),
                    archive: t.archive.clone(),
                    sha256: t.sha256.clone(),
                    error: t.error.clone(),
                })
                .collect(),
            started_unix_ms: self.started_unix_ms,
            finished_unix_ms: self.finished_unix_ms,
            verdict: crate::verdict::outcome_named(&self.verdict) as i32,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.state.as_str(), "succeeded" | "failed" | "cancelled")
    }
}

/// Where the daemon keeps every campaign's record: `<runtime>/campaigns/<id>.json`.
pub fn registry(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("campaigns")
}

pub fn read(runtime_dir: &Path, id: &str) -> Option<CampaignRecord> {
    let bytes = std::fs::read(registry(runtime_dir).join(format!("{id}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn all(runtime_dir: &Path) -> Vec<CampaignRecord> {
    let Ok(dir) = std::fs::read_dir(registry(runtime_dir)) else {
        return Vec::new();
    };
    let mut out: Vec<CampaignRecord> = dir
        .flatten()
        .filter_map(|e| serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok())
        .collect();
    out.sort_by_key(|c| c.started_unix_ms);
    out
}

/// Write the record to the registry and to `campaign.json` beside the archives, each atomically.
pub fn write(runtime_dir: &Path, rec: &CampaignRecord) {
    let bytes = serde_json::to_vec_pretty(rec).unwrap_or_default();
    for path in [
        registry(runtime_dir).join(format!("{}.json", rec.campaign_id)),
        PathBuf::from(&rec.results_dir).join("campaign.json"),
    ] {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// What the loop needs about the workspace, resolved once at `Start`.
pub struct Campaign {
    pub root: PathBuf,
    pub text: String,
    pub doc: Value,
    pub parameters: std::collections::BTreeMap<String, vxedge_core::intent::Parameter>,
    pub trials: Vec<Trial>,
    pub ingest_dir: PathBuf,
    /// The key of the host telemetry service in the experiment's `services` block.
    pub telemetry_key: String,
    pub has_traffic: bool,
    pub has_measurements: bool,
    pub unlocked: bool,
}

/// The trial's experiment document: its values bound, and the telemetry service's ingest window
/// sized to the trial.
fn trial_document(
    c: &Campaign,
    t: &Trial,
    parameters: &std::collections::BTreeMap<String, Value>,
) -> Result<String, String> {
    let mut doc = vxedge_core::campaign::bind(&c.doc, &c.parameters, parameters)?;
    let window = (t.warmup_s + t.duration_s).ceil() as u64 + 15;
    if let Some(svc) = doc
        .get_mut("services")
        .and_then(|s| s.get_mut(&c.telemetry_key))
        .and_then(Value::as_mapping_mut)
    {
        let config = svc
            .entry(Value::from("config"))
            .or_insert_with(|| Value::Mapping(Default::default()));
        if let Some(m) = config.as_mapping_mut() {
            m.insert(Value::from("duration_s"), Value::from(window));
        }
    }
    vxedge_core::campaign::with_experiment(&c.text, &doc)
}

fn phase_marker(ingest_dir: &Path, trial: &str, phase: &str, window: Option<&str>) {
    use std::io::Write;
    let mut line = serde_json::json!({
        "record": "phase", "ts_ns": now_ms() * 1_000_000, "trial": trial, "phase": phase,
    });
    if let Some(w) = window {
        line["window"] = w.into();
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ingest_dir.join("phases.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// A fresh ingest folder for a trial, holding its identity.
fn prepare_ingest(ingest_dir: &Path, campaign_id: &str, t: &Trial) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(ingest_dir);
    std::fs::create_dir_all(ingest_dir)
        .map_err(|e| format!("cannot create `{}`: {e}", ingest_dir.display()))?;
    let parameters: serde_json::Value =
        serde_json::to_value(&t.parameters).unwrap_or(serde_json::Value::Null);
    let trial = serde_json::json!({
        "campaign_id": campaign_id, "trial_id": t.id, "arm": t.arm, "repetition": t.repetition,
        "parameters": parameters, "warmup_s": t.warmup_s, "duration_s": t.duration_s,
    });
    std::fs::write(
        ingest_dir.join("trial.json"),
        serde_json::to_vec_pretty(&trial).unwrap_or_default(),
    )
    .map_err(|e| format!("cannot write the trial's identity: {e}"))
}

async fn sleep_s(s: f64) {
    if s > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(s)).await;
    }
}

/// Start the measurements for `duration_s`, wait it out, then wait for the senders' last
/// summary, a few seconds at most.
async fn measure(
    steps: &Steps,
    c: &Campaign,
    manifest: &Option<ManifestRef>,
    duration_s: f64,
    p: &Progress,
    parent: &str,
    label: &str,
) -> Result<(), JobFailure> {
    if c.has_measurements {
        let job = MeasurementService::start(
            &*steps.measure,
            stage(
                MeasurementStartRequest {
                    manifest: manifest.clone(),
                    duration_s: duration_s.ceil() as u32,
                    ..Default::default()
                },
                parent,
            ),
        )
        .await
        .map_err(|s| JobFailure::from_status(&s).context("measure start"))?
        .into_inner();
        await_job(&steps.engine, job, p, 30, &format!("{label} measure")).await?;
    }
    sleep_s(duration_s).await;
    if c.has_measurements {
        for _ in 0..20 {
            let st = MeasurementService::status(
                &*steps.measure,
                tonic::Request::new(MeasurementStatusRequest {
                    manifest: manifest.clone(),
                    ..Default::default()
                }),
            )
            .await
            .map(|r| r.into_inner())
            .ok();
            let running = st.is_some_and(|s| {
                s.measurements
                    .iter()
                    .any(|m| m.state == vxedge_api::v1::MeasurementState::Running as i32)
            });
            if !running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    Ok(())
}

async fn stop_measurements(
    steps: &Steps,
    manifest: &Option<ManifestRef>,
    p: &Progress,
    parent: &str,
    label: &str,
) -> Result<(), JobFailure> {
    let job = MeasurementService::stop(
        &*steps.measure,
        stage(
            MeasurementStopRequest {
                manifest: manifest.clone(),
                ..Default::default()
            },
            parent,
        ),
    )
    .await
    .map_err(|s| JobFailure::from_status(&s).context("measure stop"))?
    .into_inner();
    await_job(&steps.engine, job, p, 40, &format!("{label} measure stop"))
        .await
        .map(|_| ())
}

/// Run one trial and return its archive and sha256. `down` runs whenever provisioning started.
pub async fn run_trial(
    steps: &Steps,
    c: &Campaign,
    campaign_id: &str,
    t: &Trial,
    archive: &Path,
    p: &Progress,
    mut set_phase: impl FnMut(&str),
) -> Result<(String, String), JobFailure> {
    let parent = p.job_id().to_string();
    let yaml = trial_document(c, t, &t.parameters).map_err(JobFailure::from)?;
    let manifest = Some(ManifestRef {
        source: Some(Source::InlineYaml(yaml)),
        resolved_dir: c.root.display().to_string(),
        ..Default::default()
    });
    let path_ref = Some(ManifestRef {
        source: Some(Source::Path(c.root.display().to_string())),
        ..Default::default()
    });
    prepare_ingest(&c.ingest_dir, campaign_id, t).map_err(JobFailure::from)?;
    let mark = |phase: &str| phase_marker(&c.ingest_dir, &t.id, phase, None);

    set_phase("provision");
    mark("provision");
    let up = ExperimentService::up(
        &*steps.experiments,
        stage(
            ExperimentUpRequest {
                manifest: manifest.clone(),
                unlocked: c.unlocked,
                ..Default::default()
            },
            &parent,
        ),
    )
    .await
    .map_err(|s| JobFailure::from_status(&s).context(format!("trial {}: up", t.id)))?
    .into_inner();
    let result = async {
        await_job(&steps.engine, up, p, 10, &format!("{} provision", t.id)).await?;
        set_phase("warmup");
        mark("warmup");
        let load_s = (t.warmup_s + t.duration_s).ceil() as u32 + 4;
        if c.has_traffic {
            let job = TrafficService::up(
                &*steps.traffic,
                stage(
                    TrafficUpRequest {
                        manifest: manifest.clone(),
                        duration_s: load_s,
                        ..Default::default()
                    },
                    &parent,
                ),
            )
            .await
            .map_err(|s| JobFailure::from_status(&s).context("traffic up"))?
            .into_inner();
            await_job(&steps.engine, job, p, 20, &format!("{} traffic", t.id)).await?;
        }
        sleep_s(t.warmup_s).await;
        set_phase("measure");
        if t.windows.is_empty() {
            mark("measure");
            measure(steps, c, &manifest, t.duration_s, p, &parent, &t.id).await?;
        }
        // Each window runs the measurements afresh, with its own parameters, and stops them, so
        // the next window's senders start from nothing.
        for w in &t.windows {
            phase_marker(&c.ingest_dir, &t.id, "measure", Some(&w.id));
            let yaml = trial_document(c, t, &w.parameters).map_err(JobFailure::from)?;
            let window = Some(ManifestRef {
                source: Some(Source::InlineYaml(yaml)),
                resolved_dir: c.root.display().to_string(),
                ..Default::default()
            });
            let label = format!("{} {}", t.id, w.id);
            measure(steps, c, &window, w.duration_s, p, &parent, &label).await?;
            if c.has_measurements {
                stop_measurements(steps, &window, p, &parent, &label).await?;
            }
        }
        set_phase("cooldown");
        mark("cooldown");
        if c.has_measurements {
            stop_measurements(steps, &manifest, p, &parent, &t.id).await?;
        }
        if c.has_traffic {
            let job = TrafficService::down(
                &*steps.traffic,
                stage(
                    TrafficDownRequest {
                        manifest: manifest.clone(),
                        ..Default::default()
                    },
                    &parent,
                ),
            )
            .await
            .map_err(|s| JobFailure::from_status(&s).context("traffic down"))?
            .into_inner();
            await_job(&steps.engine, job, p, 45, &format!("{} traffic down", t.id)).await?;
        }
        set_phase("teardown");
        mark("teardown");
        // The ingest stops before the archive is cut, so the archive is all of it.
        let job = SvcService::down(
            &*steps.svcs,
            stage(
                SvcDownRequest {
                    service_type: "telemetry".into(),
                    experiment: manifest.clone(),
                    ..Default::default()
                },
                &parent,
            ),
        )
        .await
        .map_err(|s| JobFailure::from_status(&s).context("telemetry down"))?
        .into_inner();
        await_job(
            &steps.engine,
            job,
            p,
            50,
            &format!("{} telemetry down", t.id),
        )
        .await?;
        let job = TelemetryService::collect(
            &*steps.telemetry,
            stage(
                CollectRequest {
                    manifest: path_ref.clone(),
                    out_path: archive.display().to_string(),
                    archive_version: 2,
                },
                &parent,
            ),
        )
        .await
        .map_err(|s| JobFailure::from_status(&s).context("collect"))?
        .into_inner();
        let collect_id = job.id.clone();
        await_job(&steps.engine, job, p, 60, &format!("{} collect", t.id)).await?;
        let collected = steps.engine.get(&collect_id).await;
        let sha = collected
            .and_then(|j| j.artifact)
            .map(|a| a.sha256)
            .unwrap_or_default();
        Ok::<_, JobFailure>((archive.display().to_string(), sha))
    }
    .await;
    // Down after every trial that got this far, whatever happened.
    let down = ExperimentService::down(
        &*steps.experiments,
        stage(
            ExperimentDownRequest {
                manifest: manifest.clone(),
                ..Default::default()
            },
            &parent,
        ),
    )
    .await;
    let down = match down {
        Ok(job) => await_job(
            &steps.engine,
            job.into_inner(),
            p,
            90,
            &format!("{} down", t.id),
        )
        .await
        .map(|_| ()),
        Err(s) => Err(JobFailure::from_status(&s)),
    };
    match (result, down) {
        (Ok(done), Ok(())) => Ok(done),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e.context(format!("trial {}: down", t.id))),
    }
}
