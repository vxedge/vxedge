//! **Campaigns** (`ARCHITECTURE.md` §6.0.3): the trials an experiment's `campaign` block asks for,
//! and the document each trial runs.
//!
//! Trials (decision X-22): every point of the `sweep` is a trial, a sweep over several parameters
//! being their product, and every arm is one too. A trial whose parameter values equal an arm's
//! carries that arm's name instead of running twice. A parameter neither sets takes its default.
//! Each point runs `repetitions` times, all points of one repetition before the next, and every
//! trial is provisioned and torn down on its own (decision X-23). A `profile` overrides the
//! repetitions, warmup and duration. An experiment without a `campaign` is one point at its
//! defaults, with its `method`'s repetitions and windows.
//!
//! [`bind`] gives a trial's document: the experiment document with each parameter's value written
//! to the field it `binds`, in memory only. The package itself is never written, so its digest and
//! lock hold for the whole campaign.

use std::collections::BTreeMap;

use serde_yaml_ng::Value;

use crate::intent::{Intent, ParamType, Parameter};

/// One trial: a parameter assignment, run once.
#[derive(Debug, Clone, PartialEq)]
pub struct Trial {
    /// `r<repetition>-<point>`, unique in the campaign.
    pub id: String,
    /// The arm whose values this point equals, if any.
    pub arm: Option<String>,
    /// Every declared parameter's value for this trial.
    pub parameters: BTreeMap<String, Value>,
    /// From 1.
    pub repetition: u32,
    pub warmup_s: f64,
    pub duration_s: f64,
}

/// The trials of a campaign, and what they add up to.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    pub trials: Vec<Trial>,
    /// Distinct parameter assignments.
    pub points: u32,
    pub repetitions: u32,
    /// The profile applied, if any.
    pub profile: Option<String>,
}

/// What a trial costs besides its windows: provisioning before and teardown after, which
/// [`estimate_s`] assumes when it estimates a campaign's wall time.
pub const OVERHEAD_S: f64 = 30.0;

/// The duration a trial measures for when nothing declares one.
pub const DEFAULT_DURATION_S: f64 = 60.0;

impl Plan {
    /// The campaign's wall time, estimated as each trial's warmup and duration plus
    /// [`OVERHEAD_S`].
    pub fn estimate_s(&self) -> f64 {
        self.trials
            .iter()
            .map(|t| t.warmup_s + t.duration_s + OVERHEAD_S)
            .sum()
    }
}

/// Two values as a parameter of type `ty` compares them: numbers by value, so `0` and `0.0` agree.
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn same_point(a: &BTreeMap<String, Value>, b: &BTreeMap<String, Value>) -> bool {
    a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| same(v, w)))
}

fn label(point: &BTreeMap<String, Value>, defaults: &BTreeMap<String, Value>) -> String {
    let set: Vec<String> = point
        .iter()
        .filter(|(k, v)| !defaults.get(*k).is_some_and(|d| same(d, v)))
        .map(|(k, v)| {
            let v = serde_yaml_ng::to_string(v).unwrap_or_default();
            format!("{k}={}", v.trim())
        })
        .collect();
    if set.is_empty() {
        "defaults".to_string()
    } else {
        set.join(",")
    }
}

/// The product of the sweep's values, over `defaults`.
fn sweep_points(
    sweep: &BTreeMap<String, Vec<Value>>,
    defaults: &BTreeMap<String, Value>,
) -> Vec<BTreeMap<String, Value>> {
    let mut points = vec![defaults.clone()];
    for (param, values) in sweep {
        points = points
            .into_iter()
            .flat_map(|p| {
                values.iter().map(move |v| {
                    let mut q = p.clone();
                    q.insert(param.clone(), v.clone());
                    q
                })
            })
            .collect();
    }
    points
}

/// The trials `intent`'s campaign asks for under `profile`, with `method` (repetitions, warmup,
/// duration) for an experiment that declares no campaign. A profile the campaign does not
/// declare is an error.
pub fn plan(
    intent: &Intent,
    method: (Option<u32>, Option<f64>, Option<f64>),
    profile: Option<&str>,
) -> Result<Plan, String> {
    let defaults: BTreeMap<String, Value> = intent
        .parameters
        .iter()
        .map(|(k, p)| (k.clone(), p.default.clone()))
        .collect();
    let (mut repetitions, mut warmup_s, mut duration_s) = method;
    let mut points: Vec<(BTreeMap<String, Value>, Option<String>)> = Vec::new();
    match &intent.campaign {
        None => points.push((defaults.clone(), None)),
        Some(c) => {
            repetitions = c.repetitions.or(repetitions);
            warmup_s = c.warmup_s.or(warmup_s);
            duration_s = c.duration_s.or(duration_s);
            if !c.sweep.is_empty() {
                points.extend(
                    sweep_points(&c.sweep, &defaults)
                        .into_iter()
                        .map(|p| (p, None)),
                );
            }
            for (arm, values) in &c.arms {
                let mut p = defaults.clone();
                for (k, v) in values {
                    p.insert(k.clone(), v.clone());
                }
                match points.iter_mut().find(|(q, _)| same_point(q, &p)) {
                    Some((_, name)) if name.is_none() => *name = Some(arm.clone()),
                    // Two arms with the same values are one trial, named by the first.
                    Some(_) => {}
                    None => points.push((p, Some(arm.clone()))),
                }
            }
            if points.is_empty() {
                points.push((defaults.clone(), None));
            }
            if let Some(name) = profile {
                let p = c
                    .profiles
                    .get(name)
                    .ok_or_else(|| format!("the campaign declares no profile `{name}`"))?;
                repetitions = p.repetitions.or(repetitions);
                warmup_s = p.warmup_s.or(warmup_s);
                duration_s = p.duration_s.or(duration_s);
            }
        }
    }
    if intent.campaign.is_none()
        && let Some(name) = profile
    {
        return Err(format!(
            "the experiment declares no campaign, so it has no profile `{name}`"
        ));
    }
    let repetitions = repetitions.unwrap_or(1).max(1);
    let (warmup_s, duration_s) = (
        warmup_s.unwrap_or(0.0),
        duration_s.unwrap_or(DEFAULT_DURATION_S),
    );
    let mut trials = Vec::new();
    for rep in 1..=repetitions {
        for (point, arm) in &points {
            let name = arm.clone().unwrap_or_else(|| label(point, &defaults));
            trials.push(Trial {
                id: format!("r{rep}-{name}"),
                arm: arm.clone(),
                parameters: point.clone(),
                repetition: rep,
                warmup_s,
                duration_s,
            });
        }
    }
    Ok(Plan {
        points: points.len() as u32,
        repetitions,
        trials,
        profile: profile.map(String::from),
    })
}

/// The experiment document with each parameter's trial value written to the field it binds. A
/// parameter that binds nothing is recorded with the trial but changes no field.
pub fn bind(
    doc: &Value,
    parameters: &BTreeMap<String, Parameter>,
    values: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let mut out = doc.clone();
    for (name, p) in parameters {
        let (Some(path), Some(value)) = (p.binds.as_deref(), values.get(name)) else {
            continue;
        };
        let value = match (p.ty, value.as_f64()) {
            // A YAML integer stays an integer in an integer field.
            (ParamType::Integer, Some(x)) if x.fract() == 0.0 => Value::from(x as i64),
            _ => value.clone(),
        };
        crate::intent::set(&mut out, path, value)
            .map_err(|e| format!("parameter `{name}`: {e}"))?;
    }
    Ok(out)
}

/// A workspace's documents with the experiment document replaced by `experiment`: the text a
/// trial's model is built from.
pub fn with_experiment(text: &str, experiment: &Value) -> Result<String, String> {
    let mut out = Vec::new();
    for doc in serde_yaml_ng::Deserializer::from_str(text) {
        let v = serde::Deserialize::deserialize(doc)
            .map_err(|e: serde_yaml_ng::Error| e.to_string())?;
        let v: Value = v;
        if v.get("kind").and_then(Value::as_str) == Some("experiment") {
            out.push(experiment.clone());
        } else if !v.is_null() {
            out.push(v);
        }
    }
    let docs: Result<Vec<String>, String> = out
        .iter()
        .map(|v| serde_yaml_ng::to_string(v).map_err(|e| e.to_string()))
        .collect();
    Ok(docs?.join("---\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(yaml: &str) -> Intent {
        let doc: Value = serde_yaml_ng::from_str(yaml).unwrap();
        let (i, problems) = crate::intent::read(&doc);
        assert!(problems.is_empty(), "{problems:?}");
        i
    }

    const EXP001: &str = r#"
id: exp
traffic_profiles:
  bg: { source: n1, target: internet, bandwidth_mbps: 400 }
parameters:
  load_mbps: { type: integer, unit: Mbit/s, default: 0, min: 0, max: 1000, binds: "traffic_profiles[bg].bandwidth_mbps" }
campaign:
  arms:
    idle: { load_mbps: 0 }
    saturated: { load_mbps: 600 }
  sweep: { load_mbps: [0, 100, 250, 400, 600] }
  repetitions: 5
  warmup_s: 10
  duration_s: 60
  profiles:
    quick: { repetitions: 1, duration_s: 12 }
"#;

    #[test]
    fn exp001s_arms_label_its_sweep_points() {
        let p = plan(&intent(EXP001), (None, None, None), None).unwrap();
        assert_eq!((p.points, p.repetitions, p.trials.len()), (5, 5, 25));
        let first: Vec<&str> = p.trials[..5].iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            first,
            [
                "r1-idle",
                "r1-load_mbps=100",
                "r1-load_mbps=250",
                "r1-load_mbps=400",
                "r1-saturated"
            ]
        );
        assert_eq!(p.trials[0].arm.as_deref(), Some("idle"));
        assert_eq!(p.trials[24].id, "r5-saturated");
        assert_eq!((p.trials[0].warmup_s, p.trials[0].duration_s), (10.0, 60.0));
        assert_eq!(p.estimate_s(), 25.0 * (10.0 + 60.0 + OVERHEAD_S));
    }

    #[test]
    fn a_profile_overrides_and_an_unknown_one_is_refused() {
        let p = plan(&intent(EXP001), (None, None, None), Some("quick")).unwrap();
        assert_eq!(
            (p.trials.len(), p.trials[0].duration_s, p.trials[0].warmup_s),
            (5, 12.0, 10.0)
        );
        assert!(
            plan(&intent(EXP001), (None, None, None), Some("nope"))
                .unwrap_err()
                .contains("nope")
        );
    }

    #[test]
    fn an_arm_off_the_sweep_is_its_own_trial_and_a_sweep_over_two_is_a_product() {
        let y = r#"
id: exp
parameters:
  a: { type: integer, unit: "1", default: 0 }
  b: { type: string, default: x }
campaign:
  arms: { odd: { a: 7 } }
  sweep: { a: [1, 2], b: [x, y] }
"#;
        let p = plan(&intent(y), (None, None, None), None).unwrap();
        let ids: Vec<&str> = p.trials.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            ["r1-a=1", "r1-a=1,b=y", "r1-a=2", "r1-a=2,b=y", "r1-odd"]
        );
        assert_eq!(p.trials[4].parameters["a"], Value::from(7));
    }

    #[test]
    fn no_campaign_is_one_point_with_the_methods_windows() {
        let p = plan(&intent("id: exp\n"), (Some(3), Some(5.0), Some(30.0)), None).unwrap();
        let ids: Vec<&str> = p.trials.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["r1-defaults", "r2-defaults", "r3-defaults"]);
        assert_eq!(p.trials[0].duration_s, 30.0);
        assert!(plan(&intent("id: exp\n"), (None, None, None), Some("quick")).is_err());
    }

    #[test]
    fn bind_writes_each_value_to_its_field_and_leaves_the_rest() {
        let doc: Value = serde_yaml_ng::from_str(EXP001).unwrap();
        let i = intent(EXP001);
        let t = &plan(&i, (None, None, None), None).unwrap().trials[1];
        let bound = bind(&doc, &i.parameters, &t.parameters).unwrap();
        assert_eq!(
            crate::intent::resolve(&bound, "traffic_profiles[bg].bandwidth_mbps").unwrap(),
            &Value::from(100)
        );
        assert_eq!(
            doc["traffic_profiles"]["bg"]["bandwidth_mbps"],
            Value::from(400)
        );
    }

    #[test]
    fn the_trial_document_replaces_only_the_experiment() {
        let text =
            "kind: vehicle\nid: car\n---\nkind: experiment\nid: exp\nmethod: { duration_s: 1 }\n";
        let exp: Value =
            serde_yaml_ng::from_str("kind: experiment\nid: exp\nmethod: { duration_s: 9 }\n")
                .unwrap();
        let out = with_experiment(text, &exp).unwrap();
        assert!(
            out.contains("kind: vehicle")
                && out.contains("duration_s: 9")
                && !out.contains("duration_s: 1")
        );
    }
}
