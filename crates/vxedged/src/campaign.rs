//! `CampaignService`: the trials an experiment's campaign asks for (decisions X-22, X-23).
//! `Plan` expands them with `vxedge_core::campaign` and checks each point's bound document, so a
//! parameter value that breaks a document is a typed problem before any trial runs.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio_stream::Stream;
use tonic::{Code, Request, Response, Status};

use vxedge_api::v1::campaign_service_server::CampaignService;
use vxedge_api::v1::{
    CampaignGetRequest, CampaignList, CampaignListRequest, CampaignPlan, CampaignPlanRequest,
    CampaignStartRequest, CampaignStatus, CampaignTrial, CampaignWatchRequest, CampaignWindow, Job,
    PlanProblem,
};
use vxedge_core::campaign;

use crate::audit::Audit;
use crate::campaign_run::{self as run, CampaignRecord, Steps, TrialRecord};
use crate::errors::{self, R};
use crate::jobs::{JobFailure, JobOpts, now_ms};

pub struct CampaignSvc {
    pub steps: Steps,
    pub audit: Arc<Audit>,
    pub runtime_dir: PathBuf,
}

/// The experiment's host telemetry service: its key in `services` and its ingest folder. A
/// campaign archives every trial through it.
fn telemetry_service(
    model: &vxedge_core::model::Experiment,
    runtime_dir: &Path,
) -> Option<(String, PathBuf)> {
    model.services.iter().find_map(|(key, def)| {
        (def.service_type == vxedge_core::services::ServiceType::Telemetry && def.on.is_none())
            .then(|| {
                vxedge_core::services::telemetry_ingest_dir(def, runtime_dir)
                    .ok()
                    .map(|dir| (key.clone(), dir))
            })
            .flatten()
    })
}

/// A trial's problems in vxAPI form, under `subject`.
fn problem(reason: R, subject: &str, detail: String) -> PlanProblem {
    PlanProblem {
        reason: reason as i32,
        subject: subject.to_string(),
        detail,
    }
}

/// The campaign plan of the workspace at `root`, with every problem found.
pub(crate) fn plan_of(
    root: &std::path::Path,
    profile: Option<&str>,
) -> Result<CampaignPlan, Status> {
    let ws = vxedge_core::Workspace::load(root)
        .map_err(|e| errors::invalid(R::WorkspaceInvalid, format!("`{}`: {e}", root.display())))?;
    let doc = ws.experiments_raw.first().cloned().ok_or_else(|| {
        errors::invalid(
            R::WorkspaceInvalid,
            "a campaign needs a `kind: experiment` document",
        )
    })?;
    let subject = ws
        .experiments
        .first()
        .map(|e| e.id.clone())
        .unwrap_or_default();
    let text = vxedge_core::Workspace::read_text(root)
        .map_err(|e| errors::invalid(R::WorkspaceInvalid, e.to_string()))?;
    let model = vxedge_core::model::load_yaml(&text)
        .map_err(|e| errors::invalid(R::WorkspaceInvalid, e.to_string()))?;
    let (intent, mut intent_problems) = vxedge_core::intent::read(&doc);
    intent_problems.extend(ws.scenario_problems());
    if let Some(k) = &intent.checks {
        intent_problems.extend(vxedge_core::checks::file_problems(&subject, k, root));
    }
    let mut problems: Vec<PlanProblem> = intent_problems
        .iter()
        .map(|p| {
            let reason = match p.kind {
                vxedge_core::intent::ProblemKind::Parameter => R::ParameterInvalid,
                vxedge_core::intent::ProblemKind::Criterion => R::CriterionInvalid,
                vxedge_core::intent::ProblemKind::Measurement => R::MeasurementInvalid,
                vxedge_core::intent::ProblemKind::Scenario => R::ScenarioInvalid,
                vxedge_core::intent::ProblemKind::Check => R::CheckInvalid,
            };
            problem(reason, &subject, p.to_string())
        })
        .collect();
    let method = (
        model.method.repetitions,
        model.method.warmup_s.map(f64::from),
        model.method.duration_s.map(f64::from),
    );
    if telemetry_service(&model, std::path::Path::new("/")).is_none() {
        problems.push(problem(
            R::WorkspaceInvalid,
            &subject,
            "a campaign archives every trial through the experiment's own telemetry service: \
             declare a `services` entry of `type: telemetry` with no `on`"
                .to_string(),
        ));
    }
    let plan = match campaign::plan(&intent, method, profile) {
        Ok(p) => p,
        Err(e) => {
            problems.push(problem(
                R::ParameterInvalid,
                &subject,
                format!("campaign: {e}"),
            ));
            return Ok(CampaignPlan {
                problems,
                profile: profile.unwrap_or_default().to_string(),
                ..Default::default()
            });
        }
    };
    // Each distinct point once: its bound document must still build and pass the measurement
    // checks, since that document is what the trial runs.
    if intent_problems.is_empty() {
        let mut seen: Vec<&std::collections::BTreeMap<String, serde_yaml_ng::Value>> = Vec::new();
        for trial in &plan.trials {
            if seen.contains(&&trial.parameters) {
                continue;
            }
            seen.push(&trial.parameters);
            let point = trial
                .id
                .split_once('-')
                .map(|(_, p)| p)
                .unwrap_or(&trial.id);
            // A trial with windows runs each window's document; one without runs the point's.
            let documents: Vec<(
                String,
                &std::collections::BTreeMap<String, serde_yaml_ng::Value>,
            )> = if trial.windows.is_empty() {
                vec![(format!("point `{point}`"), &trial.parameters)]
            } else {
                trial
                    .windows
                    .iter()
                    .map(|w| (format!("point `{point}`, window `{}`", w.id), &w.parameters))
                    .collect()
            };
            for (what, values) in documents {
                let bound = campaign::bind(&doc, &intent.parameters, values)
                    .and_then(|d| campaign::with_experiment(&text, &d))
                    .and_then(|t| vxedge_core::model::load_yaml(&t).map_err(|e| e.to_string()));
                match bound {
                    Err(e) => problems.push(problem(
                        R::ParameterInvalid,
                        &subject,
                        format!("{what}: {e}"),
                    )),
                    Ok(m) => {
                        problems.extend(vxedge_core::measurements::problems(&m).into_iter().map(
                            |p| {
                                let reason = match p.kind {
                                    vxedge_core::intent::ProblemKind::Check => R::CheckInvalid,
                                    _ => R::MeasurementInvalid,
                                };
                                problem(reason, &subject, format!("{what}: {p}"))
                            },
                        ));
                        // A bound value may also break a service's config, such as a client
                        // count past what `fetch` runs.
                        problems.extend(m.services.values().filter_map(|def| {
                            vxedge_core::services::check_config(def).err().map(|e| {
                                problem(R::WorkspaceInvalid, &subject, format!("{what}: {e}"))
                            })
                        }));
                    }
                }
            }
        }
    }
    let types: std::collections::BTreeMap<&str, _> = intent
        .parameters
        .iter()
        .map(|(k, p)| (k.as_str(), p.ty))
        .collect();
    let estimated_s = plan.estimate_s();
    Ok(CampaignPlan {
        trials: plan
            .trials
            .iter()
            .map(|t| CampaignTrial {
                id: t.id.clone(),
                arm: t.arm.clone().unwrap_or_default(),
                parameters: t
                    .parameters
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            crate::package::param_value(v, types.get(k.as_str()).copied()),
                        )
                    })
                    .collect(),
                repetition: t.repetition,
                warmup_s: t.warmup_s,
                duration_s: t.duration_s,
                windows: t
                    .windows
                    .iter()
                    .map(|w| CampaignWindow {
                        id: w.id.clone(),
                        duration_s: w.duration_s,
                        parameters: w
                            .parameters
                            .iter()
                            .map(|(k, v)| {
                                (
                                    k.clone(),
                                    crate::package::param_value(v, types.get(k.as_str()).copied()),
                                )
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect(),
        points: plan.points,
        repetitions: plan.repetitions,
        estimated_s,
        problems,
        profile: plan.profile.unwrap_or_default(),
    })
}

/// A campaign id from a caller's request id and the workspace, so a retry finds the same campaign.
fn campaign_id(request_id: &str, root: &Path) -> String {
    use sha2::Digest;
    let seed = if request_id.is_empty() {
        format!("{}-{}", now_ms(), root.display())
    } else {
        format!("{request_id}-{}", root.display())
    };
    let hex = crate::services::hex::encode(sha2::Sha256::digest(seed.as_bytes()));
    format!("c-{}", &hex[..12])
}

pub(crate) fn unknown(id: &str) -> Status {
    errors::not_found(
        R::CampaignUnknown,
        format!("no campaign `{id}` on this daemon (`CampaignService.List` names them)"),
    )
}

#[tonic::async_trait]
impl CampaignService for CampaignSvc {
    type WatchStream = Pin<Box<dyn Stream<Item = Result<CampaignStatus, Status>> + Send>>;

    async fn start(&self, req: Request<CampaignStartRequest>) -> Result<Response<Job>, Status> {
        let req = req.into_inner();
        let (_, _, dir) = crate::services::resolve_manifest(req.manifest.as_ref()).await?;
        let root = dir.ok_or_else(|| {
            errors::invalid(
                R::InvalidRequest,
                "a campaign needs a workspace folder or a package, not an inline document",
            )
        })?;
        let profile = (!req.profile.is_empty()).then(|| req.profile.clone());
        let runtime_dir = self.runtime_dir.clone();
        let (r, p) = (root.clone(), profile.clone());
        let prepared = tokio::task::spawn_blocking(move || -> Result<_, Status> {
            let plan = plan_of(&r, p.as_deref())?;
            if let Some(first) = plan.problems.first() {
                let reason = R::try_from(first.reason).unwrap_or(R::WorkspaceInvalid);
                return Err(errors::precondition(reason, first.detail.clone()));
            }
            let ws = vxedge_core::Workspace::load(&r)
                .map_err(|e| errors::invalid(R::WorkspaceInvalid, e.to_string()))?;
            let text = vxedge_core::Workspace::read_text(&r)
                .map_err(|e| errors::invalid(R::WorkspaceInvalid, e.to_string()))?;
            let model = vxedge_core::model::load_yaml(&text)
                .map_err(|e| errors::invalid(R::WorkspaceInvalid, e.to_string()))?;
            let doc = ws.experiments_raw.first().cloned().unwrap_or_default();
            let (intent, _) = vxedge_core::intent::read(&doc);
            let method = (
                model.method.repetitions,
                model.method.warmup_s.map(f64::from),
                model.method.duration_s.map(f64::from),
            );
            let trials = campaign::plan(&intent, method, p.as_deref())
                .map_err(|e| errors::precondition(R::ParameterInvalid, e))?
                .trials;
            let (telemetry_key, ingest_dir) = telemetry_service(&model, &runtime_dir)
                .expect("plan_of refuses a campaign without a telemetry service");
            let digest = match &ws.package {
                Some(vxedge_core::package::PackageDoc::V1(_)) => {
                    vxedge_core::pkg::package_digest(&r).unwrap_or_default()
                }
                _ => String::new(),
            };
            Ok((
                run::Campaign {
                    root: r.clone(),
                    text,
                    doc,
                    parameters: intent.parameters,
                    trials,
                    ingest_dir,
                    telemetry_key,
                    has_traffic: !model.traffic.is_empty(),
                    has_measurements: !model.measurements.is_empty(),
                    checks: model.checks.clone(),
                    checks_out: runtime_dir.join(vxedge_core::checks::DIR),
                    unlocked: false,
                },
                model.id,
                digest,
            ))
        })
        .await
        .map_err(|e| Status::internal(format!("campaign task panicked: {e}")))??;
        let (mut campaign, experiment_id, package_digest) = prepared;
        campaign.unlocked = req.unlocked;

        let id = campaign_id(&req.request_id, &root);
        // A retry names the campaign it retries, with the same profile, or it is refused: a
        // resumed campaign is never merged with another (ARCHITECTURE.md §6.0.3).
        if let Some(previous) =
            run::read(&self.runtime_dir, &id).filter(|_| !req.request_id.is_empty())
            && previous.profile != profile.clone().unwrap_or_default()
        {
            return Err(errors::precondition(
                R::ParameterInvalid,
                format!(
                    "request_id `{}` started campaign `{id}` with profile `{}`, not `{}`; a retry \
                     uses the same profile, and another campaign another request_id",
                    req.request_id,
                    previous.profile,
                    profile.clone().unwrap_or_default()
                ),
            ));
        }

        // One campaign at a time (decision X-6). A retry of the running one gets its job.
        for active in run::all(&self.runtime_dir) {
            if active.is_terminal() {
                continue;
            }
            let job = self.steps.engine.get(&active.job_id).await;
            let live = job
                .as_ref()
                .is_some_and(|j| !crate::jobs::is_terminal(j.state));
            if live && active.campaign_id == id && !req.request_id.is_empty() {
                return Ok(Response::new(job.expect("a live job")));
            }
            if live {
                let left = active
                    .trials
                    .iter()
                    .filter(|t| t.state == "pending" || t.state == "running")
                    .count() as u64;
                return Err(errors::refuse_with(
                    Code::FailedPrecondition,
                    R::Busy,
                    format!(
                        "campaign `{}` is running ({left} trials left); one campaign runs at a time",
                        active.campaign_id
                    ),
                    &[
                        ("active_campaign", active.campaign_id.clone()),
                        ("retry_after_ms", (left * 90_000).to_string()),
                    ],
                ));
            }
        }

        let results_dir = if !req.results_dir.is_empty() {
            let d = PathBuf::from(&req.results_dir);
            if d.is_absolute() { d } else { root.join(d) }
        } else if matches!(
            req.manifest.as_ref().and_then(|m| m.source.as_ref()),
            Some(vxedge_api::v1::manifest_ref::Source::Package(_))
        ) {
            self.runtime_dir.join("campaigns").join(&id)
        } else {
            root.join("results").join(&id)
        };
        // A resumed campaign keeps the trials it finished.
        let previous = run::read(&self.runtime_dir, &id);
        let trials: Vec<TrialRecord> = campaign
            .trials
            .iter()
            .map(|t| {
                let done = previous
                    .as_ref()
                    .and_then(|p| p.trials.iter().find(|x| x.id == t.id))
                    .filter(|x| {
                        matches!(x.state.as_str(), "succeeded" | "skipped")
                            && Path::new(&x.archive).is_file()
                    });
                match done {
                    Some(x) => TrialRecord {
                        state: "skipped".into(),
                        ..x.clone()
                    },
                    None => TrialRecord {
                        id: t.id.clone(),
                        arm: t.arm.clone().unwrap_or_default(),
                        repetition: t.repetition,
                        parameters: serde_json::to_value(&t.parameters).unwrap_or_default(),
                        state: "pending".into(),
                        ..Default::default()
                    },
                }
            })
            .collect();
        self.audit.record(
            "CampaignService.Start",
            serde_json::json!({
                "campaign_id": id, "experiment_id": experiment_id, "profile": profile,
                "caller": req.caller, "unlocked": req.unlocked, "trials": trials.len(),
            }),
        );
        let reference_workspace = match crate::results::folder_of(req.reference_workspace.as_ref())?
        {
            Some(dir) => dir.display().to_string(),
            None if req.reference_workspace.is_some() => {
                return Err(errors::invalid(
                    R::WorkspaceInvalid,
                    "`reference_workspace` names a folder: a path or a package digest, not inline \
                     YAML",
                ));
            }
            None => String::new(),
        };
        let record = Arc::new(Mutex::new(CampaignRecord {
            campaign_id: id.clone(),
            state: "running".into(),
            job_id: String::new(),
            experiment_id,
            package_digest,
            profile: profile.unwrap_or_default(),
            caller: req.caller,
            unlocked: req.unlocked,
            workspace: root.display().to_string(),
            results_dir: results_dir.display().to_string(),
            reference_workspace,
            trials,
            started_unix_ms: now_ms(),
            finished_unix_ms: 0,
            verdict: String::new(),
        }));
        let (steps, runtime_dir) = (self.steps.clone(), self.runtime_dir.clone());
        let rec = record.clone();
        let job = self
            .steps
            .engine
            .submit_with(
                JobOpts::claimed_request(format!("campaign {id}"), &req.request_id),
                "campaign.run",
                move |p| async move {
                    let save = |r: &CampaignRecord| run::write(&runtime_dir, r);
                    {
                        let mut r = rec.lock().expect("campaign record");
                        r.job_id = p.job_id().to_string();
                        save(&r);
                    }
                    let total = campaign.trials.len().max(1);
                    let mut cancelled = false;
                    for (i, trial) in campaign.trials.iter().enumerate() {
                        let skip =
                            rec.lock().expect("campaign record").trials[i].state == "skipped";
                        if skip {
                            continue;
                        }
                        if p.checkpoint().is_err() {
                            cancelled = true;
                            break;
                        }
                        p.update(
                            ((i * 100) / total) as u32,
                            &format!("trial {} of {total}: {}", i + 1, trial.id),
                        );
                        {
                            let mut r = rec.lock().expect("campaign record");
                            r.trials[i].state = "running".into();
                            save(&r);
                        }
                        let archive = results_dir.join(format!("{}.zip", trial.id));
                        let set_phase = |phase: &str| {
                            let mut r = rec.lock().expect("campaign record");
                            r.trials[i].phase = phase.to_string();
                            save(&r);
                        };
                        let outcome =
                            run::run_trial(&steps, &campaign, &id, trial, &archive, &p, set_phase)
                                .await;
                        let mut r = rec.lock().expect("campaign record");
                        match outcome {
                            Ok((path, sha)) => {
                                r.trials[i].state = "succeeded".into();
                                r.trials[i].archive = path;
                                r.trials[i].sha256 = sha;
                            }
                            Err(e) => {
                                r.trials[i].state = "failed".into();
                                r.trials[i].error = e.to_string();
                            }
                        }
                        save(&r);
                    }
                    // The verdict over what was archived, from the archives, before the campaign
                    // reads as finished.
                    let snapshot = rec.lock().expect("campaign record").clone();
                    let runtime_dir = runtime_dir.clone();
                    let judged = tokio::task::spawn_blocking(move || {
                        let recorded = (!snapshot.reference_workspace.is_empty())
                            .then(|| PathBuf::from(&snapshot.reference_workspace));
                        let lookup = crate::verdict::Lookup {
                            runtime_dir: &runtime_dir,
                            workspace: recorded.as_deref(),
                        };
                        crate::verdict::of_campaign(&snapshot, &lookup)
                    })
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|v| v);
                    let mut r = rec.lock().expect("campaign record");
                    match judged {
                        Ok(v) if !v.criteria.is_empty() => {
                            r.verdict = v.outcome.as_str().to_string();
                            let path = PathBuf::from(&r.results_dir).join("verdict.json");
                            let _ = std::fs::write(
                                &path,
                                serde_json::to_vec_pretty(&v).unwrap_or_default(),
                            );
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(campaign = %id, "no verdict: {e}"),
                    }
                    let failed = r.trials.iter().filter(|t| t.state == "failed").count();
                    r.state = if cancelled {
                        "cancelled".into()
                    } else if failed > 0 {
                        "failed".into()
                    } else {
                        "succeeded".into()
                    };
                    r.finished_unix_ms = now_ms();
                    save(&r);
                    let done = r.trials.len() - failed;
                    if cancelled {
                        Err(JobFailure::cancelled())
                    } else if failed > 0 {
                        Err(JobFailure::from(format!(
                            "campaign {id}: {failed} of {} trials failed; see CampaignService.Get",
                            r.trials.len()
                        )))
                    } else {
                        Ok::<_, JobFailure>(format!(
                            "campaign {id}: {done} trials archived in {}",
                            r.results_dir
                        ))
                    }
                },
            )
            .await?;
        Ok(Response::new(job))
    }

    async fn get(
        &self,
        req: Request<CampaignGetRequest>,
    ) -> Result<Response<CampaignStatus>, Status> {
        let id = req.into_inner().campaign_id;
        run::read(&self.runtime_dir, &id)
            .map(|r| Response::new(r.status()))
            .ok_or_else(|| unknown(&id))
    }

    async fn list(
        &self,
        _: Request<CampaignListRequest>,
    ) -> Result<Response<CampaignList>, Status> {
        Ok(Response::new(CampaignList {
            campaigns: run::all(&self.runtime_dir)
                .iter()
                .map(CampaignRecord::status)
                .collect(),
        }))
    }

    async fn watch(
        &self,
        req: Request<CampaignWatchRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let id = req.into_inner().campaign_id;
        let first = run::read(&self.runtime_dir, &id).ok_or_else(|| unknown(&id))?;
        let runtime_dir = self.runtime_dir.clone();
        let stream = async_stream_watch(runtime_dir, id, first);
        Ok(Response::new(Box::pin(stream)))
    }
    async fn plan(
        &self,
        req: Request<CampaignPlanRequest>,
    ) -> Result<Response<CampaignPlan>, Status> {
        let req = req.into_inner();
        let (_, _, dir) = crate::services::resolve_manifest(req.manifest.as_ref()).await?;
        let root = dir.ok_or_else(|| {
            errors::invalid(
                R::InvalidRequest,
                "a campaign needs a workspace folder or a package, not an inline document",
            )
        })?;
        let profile = (!req.profile.is_empty()).then_some(req.profile);
        let plan = tokio::task::spawn_blocking(move || plan_of(&root, profile.as_deref()))
            .await
            .map_err(|e| Status::internal(format!("plan task panicked: {e}")))??;
        Ok(Response::new(plan))
    }
}

/// The campaign's status each time its record changes, ending once it is terminal.
fn async_stream_watch(
    runtime_dir: PathBuf,
    id: String,
    first: CampaignRecord,
) -> impl Stream<Item = Result<CampaignStatus, Status>> + Send {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        let mut last = serde_json::to_string(&first).unwrap_or_default();
        if tx.send(Ok(first.status())).await.is_err() || first.is_terminal() {
            return;
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let Some(now) = run::read(&runtime_dir, &id) else {
                return;
            };
            let text = serde_json::to_string(&now).unwrap_or_default();
            if text != last {
                last = text;
                if tx.send(Ok(now.status())).await.is_err() {
                    return;
                }
            }
            if now.is_terminal() {
                return;
            }
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}
