//! A campaign's verdict (`ARCHITECTURE.md` §6.0.3): its experiment's criteria judged over its
//! archived trials by `vxedge_core::verdict`, read again from the archives each time, so the
//! verdict says only what the archives hold.

use std::collections::BTreeMap;
use std::path::Path;

use vxedge_api::v1::{
    CriterionVerdict, ResultVerdict, TrialMetricValue, VerdictOutcome, VerdictRead, VerdictTrial,
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

/// Judge `rec`'s experiment, as its workspace holds it, over the trials it archived.
pub(crate) fn of_campaign(rec: &CampaignRecord) -> Result<Verdict, String> {
    let root = Path::new(&rec.workspace);
    let ws =
        vxedge_core::Workspace::load(root).map_err(|e| format!("`{}`: {e}", root.display()))?;
    let doc = ws
        .experiments_raw
        .first()
        .cloned()
        .ok_or("the campaign's workspace has no `kind: experiment` document")?;
    let (intent, _) = intent::read(&doc);
    let trials = rec
        .trials
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
            verdict::trial(
                Path::new(&t.archive),
                digest(&t.sha256),
                Some(who),
                &intent.metrics,
            )
            .map_err(|e| format!("trial `{}`: {e}", t.id))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(verdict::verdict(&intent, trials))
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
            })
            .collect(),
        members: j.members.iter().map(criterion).collect(),
        guards: j.guards.iter().map(criterion).collect(),
        why: j.why.clone().unwrap_or_default(),
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
            })
            .collect(),
    }
}
