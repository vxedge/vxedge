//! A campaign's verdict (`ARCHITECTURE.md` §6.0.3): its experiment's criteria judged over its
//! archived trials by `vxedge_core::verdict`, read again from the archives each time, so the
//! verdict says only what the archives hold.

use std::collections::BTreeMap;
use std::path::Path;

use vxedge_api::v1::{
    CriterionVerdict, ResultVerdict, TrialMetricValue, VerdictCase, VerdictOutcome, VerdictRead,
    VerdictTrial,
};
use vxedge_core::intent::{self, Judgement, Outcome};
use vxedge_core::verdict::{self, Identity, Verdict};

use crate::campaign_run::CampaignRecord;
use crate::package::param_value;

/// The archive's digest as vxAPI names it.
fn digest(sha256: &str) -> String {
    if sha256.is_empty() || sha256.starts_with("sha256:") {
        sha256.to_string()
    } else {
        format!("sha256:{sha256}")
    }
}

/// The typed intent of `rec`'s experiment, as its workspace holds it.
fn intent_of(rec: &CampaignRecord) -> Result<intent::Intent, String> {
    let root = Path::new(&rec.workspace);
    let ws =
        vxedge_core::Workspace::load(root).map_err(|e| format!("`{}`: {e}", root.display()))?;
    let doc = ws
        .experiments_raw
        .first()
        .cloned()
        .ok_or("the campaign's workspace has no `kind: experiment` document")?;
    Ok(intent::read(&doc).0)
}

/// The trials `rec` archived, each with `intent`'s metrics read from its archive.
fn trials_of(rec: &CampaignRecord, intent: &intent::Intent) -> Result<Vec<verdict::Trial>, String> {
    rec.trials
        .iter()
        .filter(|t| t.state == "succeeded" && !t.archive.is_empty())
        .map(|t| {
            let parameters = t
                .parameters
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), serde_yaml_ng::to_value(v).ok()?)))
                        .collect()
                })
                .unwrap_or_default();
            let who = Identity {
                id: t.id.clone(),
                arm: (!t.arm.is_empty()).then(|| t.arm.clone()),
                repetition: t.repetition,
                parameters,
            };
            verdict::trial(Path::new(&t.archive), digest(&t.sha256), Some(who), intent)
                .map_err(|e| format!("campaign `{}`, trial `{}`: {e}", rec.campaign_id, t.id))
        })
        .collect()
}

/// Where a verdict finds the reference campaigns an experiment declares: in `workspace`'s
/// `results/<campaign>/campaign.json` when given, else among this host's campaigns, else in the
/// judged campaign's own workspace's `results/`.
pub(crate) struct Lookup<'a> {
    pub runtime_dir: &'a Path,
    pub workspace: Option<&'a Path>,
}

/// The trials of each reference `intent` declares, read with `intent`'s metrics.
fn references(
    intent: &intent::Intent,
    rec: &CampaignRecord,
    lookup: &Lookup<'_>,
) -> verdict::References {
    intent
        .references
        .iter()
        .map(|(name, r)| {
            let id = &r.campaign;
            let found = match lookup.workspace {
                Some(root) => {
                    crate::results::recorded_in(root, id).map_err(|s| s.message().to_string())
                }
                None => crate::campaign_run::read(lookup.runtime_dir, id)
                    .map(Ok)
                    .unwrap_or_else(|| {
                        crate::results::recorded_in(Path::new(&rec.workspace), id).map_err(|_| {
                            format!(
                                "campaign `{id}` is not on this host or in the campaign's \
                                 workspace; give the workspace that records it \
                                 (`reference_workspace`)"
                            )
                        })
                    }),
            };
            (name.clone(), found.and_then(|r| trials_of(&r, intent)))
        })
        .collect()
}

/// Judge `rec`'s experiment, as its workspace holds it, over the trials it archived.
pub(crate) fn of_campaign(rec: &CampaignRecord, lookup: &Lookup<'_>) -> Result<Verdict, String> {
    let intent = intent_of(rec)?;
    let refs = references(&intent, rec, lookup);
    Ok(verdict::verdict_with(
        &intent,
        trials_of(rec, &intent)?,
        &refs,
    ))
}

/// `rec` against `reference`, both judged by `rec`'s experiment.
pub(crate) fn compare(
    rec: &CampaignRecord,
    reference: &CampaignRecord,
    lookup: &Lookup<'_>,
) -> Result<verdict::Comparison, String> {
    let intent = intent_of(rec)?;
    let refs = references(&intent, rec, lookup);
    Ok(verdict::compare_with(
        &intent,
        trials_of(rec, &intent)?,
        trials_of(reference, &intent)?,
        &refs,
    ))
}

/// `c` as vxAPI's `ResultComparison`.
pub(crate) fn comparison_to_proto(
    rec: &CampaignRecord,
    reference: &CampaignRecord,
    c: &verdict::Comparison,
) -> vxedge_api::v1::ResultComparison {
    use vxedge_api::v1::{CriterionDelta, MetricDelta};
    vxedge_api::v1::ResultComparison {
        campaign_id: rec.campaign_id.clone(),
        reference_campaign_id: reference.campaign_id.clone(),
        outcome: outcome(c.outcome) as i32,
        reference_outcome: outcome(c.reference_outcome) as i32,
        metrics: c
            .metrics
            .iter()
            .map(|m| MetricDelta {
                metric: m.metric.clone(),
                point: m.point.clone(),
                count: m.count as u32,
                reference_count: m.reference_count as u32,
                median: m.median,
                iqr: m.iqr,
                reference_median: m.reference_median,
                reference_iqr: m.reference_iqr,
                delta: m.delta,
                within_reference_iqr: m.within_reference_iqr,
            })
            .collect(),
        criteria: c
            .criteria
            .iter()
            .map(|d| CriterionDelta {
                id: d.id.clone(),
                outcome: outcome(d.outcome) as i32,
                reference_outcome: outcome(d.reference_outcome) as i32,
            })
            .collect(),
        within: c.within as u32,
    }
}

pub(crate) fn outcome(o: Outcome) -> VerdictOutcome {
    match o {
        Outcome::Pass => VerdictOutcome::Pass,
        Outcome::Fail => VerdictOutcome::Fail,
        Outcome::Inconclusive => VerdictOutcome::Inconclusive,
    }
}

/// `pass`, `fail` or `inconclusive` from `campaign.json`, typed.
pub(crate) fn outcome_named(name: &str) -> VerdictOutcome {
    match name {
        "pass" => VerdictOutcome::Pass,
        "fail" => VerdictOutcome::Fail,
        "inconclusive" => VerdictOutcome::Inconclusive,
        _ => VerdictOutcome::Unspecified,
    }
}

fn params(
    m: &BTreeMap<String, serde_yaml_ng::Value>,
) -> std::collections::HashMap<String, vxedge_api::v1::ParamValue> {
    m.iter()
        .map(|(k, v)| (k.clone(), param_value(v, None)))
        .collect()
}

fn case(case: &str, trial: &str, r: &vxedge_core::checks::CaseResult) -> VerdictCase {
    VerdictCase {
        case_id: case.to_string(),
        trial_id: trial.to_string(),
        outcome: r.outcome.map_or(VerdictOutcome::Unspecified, outcome) as i32,
        value_json: r.value.as_ref().map(|v| v.to_string()).unwrap_or_default(),
        why: r.why.clone().unwrap_or_default(),
    }
}

fn case_read(r: &intent::CaseRead) -> VerdictCase {
    case(&r.case, &r.trial, &r.result)
}

fn criterion(j: &Judgement) -> CriterionVerdict {
    CriterionVerdict {
        id: j.id.clone().unwrap_or_default(),
        outcome: outcome(j.outcome) as i32,
        rule: j.rule.clone(),
        lhs: j.lhs,
        rhs: j.rhs,
        tolerance: j.tolerance,
        reads: j
            .reads
            .iter()
            .map(|r| VerdictRead {
                metric: r.metric.clone(),
                aggregate: r.aggregate.clone(),
                select: params(&r.select),
                trials: r.trials.clone(),
                values: r.values.clone(),
                complete: r.complete,
                value: r.value,
                reference: r.reference.clone().unwrap_or_default(),
            })
            .collect(),
        members: j.members.iter().map(criterion).collect(),
        guards: j.guards.iter().map(criterion).collect(),
        why: j.why.clone().unwrap_or_default(),
        cases: j.cases.iter().map(case_read).collect(),
    }
}

/// `v` as vxAPI's `ResultVerdict`.
pub(crate) fn to_proto(rec: &CampaignRecord, v: &Verdict) -> ResultVerdict {
    ResultVerdict {
        campaign_id: rec.campaign_id.clone(),
        experiment_id: rec.experiment_id.clone(),
        hypothesis: v.hypothesis.clone().unwrap_or_default(),
        outcome: outcome(v.outcome) as i32,
        criteria: v.criteria.iter().map(criterion).collect(),
        observations: v.observations.iter().map(case_read).collect(),
        trials: v
            .trials
            .iter()
            .map(|t| VerdictTrial {
                trial_id: t.id.clone(),
                arm: t.arm.clone().unwrap_or_default(),
                repetition: t.repetition,
                parameters: params(&t.parameters),
                archive_digest: t.archive.clone(),
                values: t
                    .values
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            TrialMetricValue {
                                value: v.value,
                                complete: v.complete,
                                why: v.why.clone().unwrap_or_default(),
                            },
                        )
                    })
                    .collect(),
                cases: t
                    .cases
                    .iter()
                    .map(|(k, r)| (k.clone(), case(k, &t.id, r)))
                    .collect(),
            })
            .collect(),
    }
}
