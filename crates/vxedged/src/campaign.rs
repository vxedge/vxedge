//! `CampaignService`: the trials an experiment's campaign asks for (decisions X-22, X-23).
//! `Plan` expands them with `vxedge_core::campaign` and checks each point's bound document, so a
//! parameter value that breaks a document is a typed problem before any trial runs.

use tonic::{Request, Response, Status};

use vxedge_api::v1::campaign_service_server::CampaignService;
use vxedge_api::v1::{CampaignPlan, CampaignPlanRequest, CampaignTrial, PlanProblem};
use vxedge_core::campaign;

use crate::errors::{self, R};

pub struct CampaignSvc;

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
    let (intent, intent_problems) = vxedge_core::intent::read(&doc);
    let mut problems: Vec<PlanProblem> = intent_problems
        .iter()
        .map(|p| {
            let reason = match p.kind {
                vxedge_core::intent::ProblemKind::Parameter => R::ParameterInvalid,
                vxedge_core::intent::ProblemKind::Criterion => R::CriterionInvalid,
                vxedge_core::intent::ProblemKind::Measurement => R::MeasurementInvalid,
            };
            problem(reason, &subject, p.to_string())
        })
        .collect();
    let method = (
        model.method.repetitions,
        model.method.warmup_s.map(f64::from),
        model.method.duration_s.map(f64::from),
    );
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
            let bound = campaign::bind(&doc, &intent.parameters, &trial.parameters)
                .and_then(|d| campaign::with_experiment(&text, &d))
                .and_then(|t| vxedge_core::model::load_yaml(&t).map_err(|e| e.to_string()));
            match bound {
                Err(e) => problems.push(problem(
                    R::ParameterInvalid,
                    &subject,
                    format!("point `{point}`: {e}"),
                )),
                Ok(m) => problems.extend(vxedge_core::measurements::problems(&m).into_iter().map(
                    |p| {
                        problem(
                            R::MeasurementInvalid,
                            &subject,
                            format!("point `{point}`: {p}"),
                        )
                    },
                )),
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
            })
            .collect(),
        points: plan.points,
        repetitions: plan.repetitions,
        estimated_s,
        problems,
        profile: plan.profile.unwrap_or_default(),
    })
}

#[tonic::async_trait]
impl CampaignService for CampaignSvc {
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
