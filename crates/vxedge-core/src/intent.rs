//! **Typed intent** (`ARCHITECTURE.md` §6.0.3): what an experiment tests and how a result is
//! judged, checked before any trial runs.
//!
//! Every block is optional; an experiment without them reads exactly as before. The blocks are
//! read from the experiment document as written: `binds` paths resolve against that document, so
//! no text is ever templated and every document stays plain YAML with the defaults in place.
//! [`check`] returns every problem, typed as a parameter or a criterion problem, so `Plan` can name
//! them all and `up` can refuse on the first. [`evaluate`] judges a criterion over samples and
//! never rounds `inconclusive` to a pass.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_yaml_ng::Value;

/// Which kind of problem: a parameter (including the campaign that sets parameters), a
/// criterion, or a measurement ([`crate::measurements`]). vxAPI reports them as
/// `ERROR_REASON_PARAMETER_INVALID`, `ERROR_REASON_CRITERION_INVALID` and
/// `ERROR_REASON_MEASUREMENT_INVALID`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemKind {
    Parameter,
    Criterion,
    Measurement,
}

/// One problem with an experiment's typed intent: the field it is in, as a path from the
/// document's top, and what is wrong.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("experiment `{experiment}`, `{field}`: {detail}")]
pub struct Problem {
    pub kind: ProblemKind,
    pub experiment: String,
    pub field: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hypothesis {
    pub id: String,
    pub statement: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    Integer,
    Number,
    Boolean,
    String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    #[serde(rename = "type")]
    pub ty: ParamType,
    #[serde(default)]
    pub unit: Option<String>,
    pub default: Value,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub values: Option<Vec<Value>>,
    /// The manifest field this parameter sets, by id path.
    #[serde(default)]
    pub binds: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

impl MetricKind {
    /// The aggregates a metric of this kind has.
    pub fn aggregates(self) -> &'static [&'static str] {
        match self {
            MetricKind::Histogram => &["mean", "min", "max", "p50", "p90", "p95", "p99", "count"],
            MetricKind::Gauge => &["mean", "min", "max", "last"],
            MetricKind::Counter => &["total", "max", "last"],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    pub id: String,
    pub kind: MetricKind,
    pub unit: String,
    pub entity: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Within,
}

/// One side of a criterion: an aggregate of a metric's samples, or a constant.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Operand {
    Metric(MetricOperand),
    Constant(Constant),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricOperand {
    pub metric: String,
    pub aggregate: String,
    /// `arm: <name>`, or parameter values such as `load_mbps: 600`.
    #[serde(default)]
    pub select: BTreeMap<String, Value>,
    #[serde(default)]
    pub factor: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constant {
    pub value: f64,
    pub unit: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comparison {
    pub id: String,
    pub op: Op,
    pub lhs: Operand,
    pub rhs: Operand,
    /// For `within`: the most the two operands may differ by, in their unit.
    #[serde(default)]
    pub tolerance: Option<f64>,
    #[serde(default)]
    pub min_samples: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub all: Option<Vec<Criterion>>,
    #[serde(default)]
    pub any: Option<Vec<Criterion>>,
}

/// A criterion: a comparison of two operands, or an `all`/`any` group of criteria.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Criterion {
    Comparison(Comparison),
    Group(Group),
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default)]
    pub repetitions: Option<u32>,
    #[serde(default)]
    pub warmup_s: Option<f64>,
    #[serde(default)]
    pub duration_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Campaign {
    #[serde(default)]
    pub arms: BTreeMap<String, BTreeMap<String, Value>>,
    #[serde(default)]
    pub sweep: BTreeMap<String, Vec<Value>>,
    #[serde(default)]
    pub repetitions: Option<u32>,
    #[serde(default)]
    pub warmup_s: Option<f64>,
    #[serde(default)]
    pub duration_s: Option<f64>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// An experiment's typed intent, as far as it parses.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Intent {
    pub hypothesis: Option<Hypothesis>,
    pub parameters: BTreeMap<String, Parameter>,
    pub metrics: Vec<Metric>,
    pub criteria: Vec<Criterion>,
    pub campaign: Option<Campaign>,
}

struct Checker<'a> {
    experiment: String,
    doc: &'a Value,
    problems: Vec<Problem>,
}

impl Checker<'_> {
    fn push(&mut self, kind: ProblemKind, field: impl Into<String>, detail: impl Into<String>) {
        self.problems.push(Problem {
            kind,
            experiment: self.experiment.clone(),
            field: field.into(),
            detail: detail.into(),
        });
    }

    fn section<T: serde::de::DeserializeOwned>(
        &mut self,
        key: &str,
        kind: ProblemKind,
    ) -> Option<T> {
        let value = self.doc.get(key)?;
        match serde_yaml_ng::from_value(value.clone()) {
            Ok(v) => Some(v),
            Err(e) => {
                self.push(kind, key, e.to_string());
                None
            }
        }
    }
}

/// The experiment document's typed intent and every problem with it. An experiment with no
/// typed intent has none.
pub fn read(doc: &Value) -> (Intent, Vec<Problem>) {
    let experiment = doc
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    let mut c = Checker {
        experiment,
        doc,
        problems: Vec::new(),
    };
    let intent = Intent {
        hypothesis: c.section("hypothesis", ProblemKind::Parameter),
        parameters: c
            .section("parameters", ProblemKind::Parameter)
            .unwrap_or_default(),
        metrics: c
            .section("metrics", ProblemKind::Criterion)
            .unwrap_or_default(),
        criteria: c
            .section("criteria", ProblemKind::Criterion)
            .unwrap_or_default(),
        campaign: c.section("campaign", ProblemKind::Parameter),
    };
    check_parameters(&mut c, &intent);
    check_campaign(&mut c, &intent);
    campaign_and_method(&mut c);
    check_metrics(&mut c, &intent);
    let mut ids = BTreeSet::new();
    for (i, criterion) in intent.criteria.iter().enumerate() {
        check_criterion(
            &mut c,
            &intent,
            criterion,
            &format!("criteria[{i}]"),
            &mut ids,
        );
    }
    (intent, c.problems)
}

/// Every problem with the experiment document's typed intent; [`read`] without the intent.
pub fn check(doc: &Value) -> Vec<Problem> {
    read(doc).1
}

/// `campaign` replaces `method`, and an experiment declares one of them. Kept as its own rule so
/// it can change on its own.
fn campaign_and_method(c: &mut Checker<'_>) {
    if c.doc.get("campaign").is_some() && c.doc.get("method").is_some() {
        c.push(
            ProblemKind::Parameter,
            "campaign",
            "an experiment declares `campaign` or `method`, not both: `campaign` replaces `method`",
        );
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "an integer",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Sequence(_) => "a list",
        Value::Mapping(_) => "a map",
        Value::Tagged(_) => "a tagged value",
    }
}

fn has_type(v: &Value, ty: ParamType) -> bool {
    match ty {
        ParamType::Integer => v.as_i64().is_some() || v.as_u64().is_some(),
        ParamType::Number => v.as_f64().is_some(),
        ParamType::Boolean => v.as_bool().is_some(),
        ParamType::String => v.as_str().is_some(),
    }
}

/// Why `value` is not a valid value of `p`, if it is not.
fn value_problem(p: &Parameter, value: &Value) -> Option<String> {
    if !has_type(value, p.ty) {
        return Some(format!(
            "`{}` is {}, and the parameter is {:?}",
            serde_yaml_ng::to_string(value).unwrap_or_default().trim(),
            type_name(value),
            p.ty
        ));
    }
    if let Some(x) = value.as_f64()
        && (p.min.is_some_and(|m| x < m) || p.max.is_some_and(|m| x > m))
    {
        return Some(format!(
            "{x} is outside [{}, {}]",
            p.min.map_or("-inf".into(), |m| m.to_string()),
            p.max.map_or("inf".into(), |m| m.to_string())
        ));
    }
    if let Some(allowed) = &p.values
        && !allowed.contains(value)
    {
        return Some(format!(
            "`{}` is not one of the allowed values",
            serde_yaml_ng::to_string(value).unwrap_or_default().trim()
        ));
    }
    None
}

/// One step of an id path: a field name, optionally selecting the list element whose `id`, or
/// the map entry whose key, is the bracketed name.
fn parse_path(path: &str) -> Option<Vec<(&str, Option<&str>)>> {
    path.split('.')
        .map(|seg| match seg.split_once('[') {
            Some((name, rest)) => {
                let key = rest.strip_suffix(']')?;
                (!name.is_empty() && !key.is_empty() && !key.contains(['[', ']']))
                    .then_some((name, Some(key)))
            }
            None => (!seg.is_empty() && !seg.contains(']')).then_some((seg, None)),
        })
        .collect()
}

/// The field an id path names in `doc`, or why it names none. Pure.
pub fn resolve<'a>(doc: &'a Value, path: &str) -> Result<&'a Value, String> {
    let steps = parse_path(path).ok_or_else(|| {
        format!("`{path}` is not an id path such as `traffic_profiles[bg].bandwidth_mbps`")
    })?;
    let mut here = doc;
    let mut walked = String::new();
    for (name, key) in steps {
        if !walked.is_empty() {
            walked.push('.');
        }
        walked.push_str(name);
        here = here
            .get(name)
            .ok_or_else(|| format!("`{walked}` does not exist in the experiment document"))?;
        if let Some(key) = key {
            walked.push_str(&format!("[{key}]"));
            here = match here {
                Value::Sequence(items) => items
                    .iter()
                    .find(|i| i.get("id").and_then(Value::as_str) == Some(key))
                    .ok_or_else(|| format!("`{walked}`: no element has `id: {key}`"))?,
                Value::Mapping(_) => here
                    .get(key)
                    .ok_or_else(|| format!("`{walked}`: no entry `{key}`"))?,
                _ => return Err(format!("`{walked}` is not a list or a map")),
            };
        }
    }
    Ok(here)
}

/// Set the field an id path names in `doc` to `value`, or say why it names none. The field must
/// already exist: a parameter binds a field the document declares, so the bound document keeps
/// the shape it validated with.
pub fn set(doc: &mut Value, path: &str, value: Value) -> Result<(), String> {
    let steps = parse_path(path).ok_or_else(|| format!("`{path}` is not an id path"))?;
    let mut here = doc;
    for (name, key) in steps {
        here = here
            .get_mut(name)
            .ok_or_else(|| format!("`{path}`: `{name}` does not exist"))?;
        if let Some(key) = key {
            here = match here {
                Value::Sequence(items) => items
                    .iter_mut()
                    .find(|i| i.get("id").and_then(Value::as_str) == Some(key))
                    .ok_or_else(|| format!("`{path}`: no element has `id: {key}`"))?,
                Value::Mapping(_) => here
                    .get_mut(key)
                    .ok_or_else(|| format!("`{path}`: no entry `{key}`"))?,
                _ => return Err(format!("`{path}`: `{name}` is not a list or a map")),
            };
        }
    }
    *here = value;
    Ok(())
}

fn check_parameters(c: &mut Checker<'_>, intent: &Intent) {
    let mut bound: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, p) in &intent.parameters {
        let field = |f: &str| format!("parameters.{name}.{f}");
        let numeric = matches!(p.ty, ParamType::Integer | ParamType::Number);
        if numeric && p.unit.as_deref().is_none_or(str::is_empty) {
            c.push(
                ProblemKind::Parameter,
                field("unit"),
                "a numeric parameter needs a `unit`",
            );
        }
        if !numeric && (p.min.is_some() || p.max.is_some()) {
            c.push(
                ProblemKind::Parameter,
                field("min"),
                "`min` and `max` apply to numeric parameters only",
            );
        }
        if let (Some(lo), Some(hi)) = (p.min, p.max)
            && lo > hi
        {
            c.push(
                ProblemKind::Parameter,
                field("min"),
                format!("`min` {lo} is above `max` {hi}"),
            );
        }
        if let Some(why) = value_problem(p, &p.default) {
            c.push(ProblemKind::Parameter, field("default"), why);
        }
        let Some(path) = p.binds.as_deref() else {
            continue;
        };
        match resolve(c.doc, path) {
            Err(why) => c.push(ProblemKind::Parameter, field("binds"), why),
            Ok(target) if !has_type(target, p.ty) => c.push(
                ProblemKind::Parameter,
                field("binds"),
                format!(
                    "`{path}` is {}, and the parameter is {:?}",
                    type_name(target),
                    p.ty
                ),
            ),
            Ok(_) => {}
        }
        if let Some(other) = bound.insert(path, name) {
            c.push(
                ProblemKind::Parameter,
                field("binds"),
                format!("`{path}` is already bound by parameter `{other}`"),
            );
        }
    }
}

fn check_campaign(c: &mut Checker<'_>, intent: &Intent) {
    let Some(campaign) = &intent.campaign else {
        return;
    };
    let set = |c: &mut Checker<'_>, field: String, param: &str, value: &Value| match intent
        .parameters
        .get(param)
    {
        None => c.push(
            ProblemKind::Parameter,
            field,
            format!("`{param}` is not a declared parameter"),
        ),
        Some(p) => {
            if let Some(why) = value_problem(p, value) {
                c.push(ProblemKind::Parameter, field, why);
            }
        }
    };
    for (arm, values) in &campaign.arms {
        for (param, value) in values {
            set(c, format!("campaign.arms.{arm}.{param}"), param, value);
        }
    }
    for (param, values) in &campaign.sweep {
        if values.is_empty() {
            c.push(
                ProblemKind::Parameter,
                format!("campaign.sweep.{param}"),
                "a sweep needs values",
            );
        }
        for (i, value) in values.iter().enumerate() {
            set(c, format!("campaign.sweep.{param}[{i}]"), param, value);
        }
    }
    if campaign.repetitions == Some(0) {
        c.push(
            ProblemKind::Parameter,
            "campaign.repetitions",
            "needs at least 1",
        );
    }
}

fn check_metrics(c: &mut Checker<'_>, intent: &Intent) {
    let mut seen = BTreeSet::new();
    for (i, m) in intent.metrics.iter().enumerate() {
        if !seen.insert(m.id.as_str()) {
            c.push(
                ProblemKind::Criterion,
                format!("metrics[{i}].id"),
                format!("`{}` is declared twice", m.id),
            );
        }
        if m.unit.is_empty() {
            c.push(
                ProblemKind::Criterion,
                format!("metrics[{i}].unit"),
                "a metric needs a `unit`",
            );
        }
    }
}

/// The unit an operand is in, once checked.
fn check_operand(
    c: &mut Checker<'_>,
    intent: &Intent,
    operand: &Operand,
    field: &str,
) -> Option<String> {
    match operand {
        Operand::Constant(k) => Some(k.unit.clone()),
        Operand::Metric(m) => {
            let Some(metric) = intent.metrics.iter().find(|x| x.id == m.metric) else {
                c.push(
                    ProblemKind::Criterion,
                    format!("{field}.metric"),
                    format!("`{}` is not a declared metric", m.metric),
                );
                return None;
            };
            if !metric.kind.aggregates().contains(&m.aggregate.as_str()) {
                c.push(
                    ProblemKind::Criterion,
                    format!("{field}.aggregate"),
                    format!(
                        "a {:?} metric has no `{}`; it has {}",
                        metric.kind,
                        m.aggregate,
                        metric.kind.aggregates().join(", ")
                    ),
                );
            }
            for (key, value) in &m.select {
                let at = format!("{field}.select.{key}");
                if key == "arm" {
                    // A campaign that does not read is reported on its own; its arms are unknown,
                    // not missing.
                    if intent.campaign.is_none() && c.doc.get("campaign").is_some() {
                        continue;
                    }
                    let arm = value.as_str().unwrap_or_default();
                    let known = intent
                        .campaign
                        .as_ref()
                        .is_some_and(|camp| camp.arms.contains_key(arm));
                    if !known {
                        c.push(
                            ProblemKind::Criterion,
                            at,
                            format!("`{arm}` is not an arm of the campaign"),
                        );
                    }
                } else {
                    match intent.parameters.get(key) {
                        None => c.push(
                            ProblemKind::Criterion,
                            at,
                            format!("`{key}` is neither `arm` nor a declared parameter"),
                        ),
                        Some(p) => {
                            if let Some(why) = value_problem(p, value) {
                                c.push(ProblemKind::Criterion, at, why);
                            }
                        }
                    }
                }
            }
            Some(metric.unit.clone())
        }
    }
}

fn check_criterion(
    c: &mut Checker<'_>,
    intent: &Intent,
    criterion: &Criterion,
    field: &str,
    ids: &mut BTreeSet<String>,
) {
    let mut unique = |c: &mut Checker<'_>, id: &str| {
        if !ids.insert(id.to_string()) {
            c.push(
                ProblemKind::Criterion,
                format!("{field}.id"),
                format!("`{id}` is used twice"),
            );
        }
    };
    match criterion {
        Criterion::Group(g) => {
            if let Some(id) = &g.id {
                unique(c, id);
            }
            let members = match (&g.all, &g.any) {
                (Some(m), None) | (None, Some(m)) => m,
                _ => {
                    c.push(
                        ProblemKind::Criterion,
                        field,
                        "a group has exactly one of `all` or `any`",
                    );
                    return;
                }
            };
            if members.is_empty() {
                c.push(ProblemKind::Criterion, field, "a group needs members");
            }
            let key = if g.all.is_some() { "all" } else { "any" };
            for (i, m) in members.iter().enumerate() {
                check_criterion(c, intent, m, &format!("{field}.{key}[{i}]"), ids);
            }
        }
        Criterion::Comparison(k) => {
            unique(c, &k.id);
            let lhs = check_operand(c, intent, &k.lhs, &format!("{field}.lhs"));
            let rhs = check_operand(c, intent, &k.rhs, &format!("{field}.rhs"));
            if let (Some(l), Some(r)) = (&lhs, &rhs)
                && l != r
            {
                c.push(
                    ProblemKind::Criterion,
                    field,
                    format!("compares `{l}` with `{r}`; the operands' units must agree"),
                );
            }
            match (k.op, k.tolerance) {
                (Op::Within, None) => c.push(
                    ProblemKind::Criterion,
                    format!("{field}.tolerance"),
                    "`within` needs a `tolerance`",
                ),
                (Op::Within, Some(t)) if t < 0.0 => c.push(
                    ProblemKind::Criterion,
                    format!("{field}.tolerance"),
                    "cannot be negative",
                ),
                (op, Some(_)) if op != Op::Within => c.push(
                    ProblemKind::Criterion,
                    format!("{field}.tolerance"),
                    "only `within` takes a `tolerance`",
                ),
                _ => {}
            }
            if matches!(k.lhs, Operand::Constant(_)) && matches!(k.rhs, Operand::Constant(_)) {
                c.push(
                    ProblemKind::Criterion,
                    field,
                    "compares two constants; nothing is measured",
                );
            }
        }
    }
}

/// What a criterion found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail,
    Inconclusive,
}

/// The samples of one metric under one selection, and whether its telemetry was complete.
#[derive(Debug, Clone, PartialEq)]
pub struct Samples {
    pub values: Vec<f64>,
    pub complete: bool,
}

/// Where [`evaluate`] reads samples from: results, or a live recording.
pub trait SampleSource {
    /// The samples of `metric` under `select`, or `None` when there are none at all.
    fn samples(&self, metric: &str, select: &BTreeMap<String, Value>) -> Option<Samples>;
}

/// `aggregate` of `values`, which are not empty. Percentiles are nearest-rank.
pub fn aggregate(values: &[f64], aggregate: &str) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = |p: f64| {
        let i = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
        sorted[i.clamp(1, sorted.len()) - 1]
    };
    Some(match aggregate {
        "mean" => values.iter().sum::<f64>() / values.len() as f64,
        "min" => sorted[0],
        "max" | "total" => sorted[sorted.len() - 1],
        "last" => values[values.len() - 1],
        "count" => values.len() as f64,
        "p50" => rank(50.0),
        "p90" => rank(90.0),
        "p95" => rank(95.0),
        "p99" => rank(99.0),
        _ => return None,
    })
}

fn operand_value(operand: &Operand, min_samples: u64, src: &dyn SampleSource) -> Option<f64> {
    match operand {
        Operand::Constant(k) => Some(k.value),
        Operand::Metric(m) => {
            let s = src.samples(&m.metric, &m.select)?;
            if !s.complete || (s.values.len() as u64) < min_samples.max(1) {
                return None;
            }
            aggregate(&s.values, &m.aggregate).map(|v| v * m.factor.unwrap_or(1.0))
        }
    }
}

/// Judge `criterion` over `src`. A comparison is `inconclusive` when an operand has no samples,
/// fewer than `min_samples`, or incomplete telemetry. `all` fails on any conclusive failure and
/// `any` passes on any conclusive pass; otherwise a group that has an inconclusive member is
/// inconclusive. `inconclusive` is never turned into a pass.
pub fn evaluate(criterion: &Criterion, src: &dyn SampleSource) -> Outcome {
    match criterion {
        Criterion::Comparison(k) => {
            let n = k.min_samples.unwrap_or(1);
            let (Some(l), Some(r)) = (operand_value(&k.lhs, n, src), operand_value(&k.rhs, n, src))
            else {
                return Outcome::Inconclusive;
            };
            let holds = match k.op {
                Op::Lt => l < r,
                Op::Le => l <= r,
                Op::Gt => l > r,
                Op::Ge => l >= r,
                Op::Within => (l - r).abs() <= k.tolerance.unwrap_or(0.0),
            };
            if holds { Outcome::Pass } else { Outcome::Fail }
        }
        Criterion::Group(g) => {
            let (members, all) = match (&g.all, &g.any) {
                (Some(m), _) => (m, true),
                (None, Some(m)) => (m, false),
                (None, None) => return Outcome::Inconclusive,
            };
            let outcomes: Vec<Outcome> = members.iter().map(|m| evaluate(m, src)).collect();
            let decisive = if all { Outcome::Fail } else { Outcome::Pass };
            if outcomes.contains(&decisive) {
                decisive
            } else if outcomes.contains(&Outcome::Inconclusive) || outcomes.is_empty() {
                Outcome::Inconclusive
            } else if all {
                Outcome::Pass
            } else {
                Outcome::Fail
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// exp-001's typed intent, as the spec's example writes it.
    const EXP: &str = r#"
kind: experiment
id: exp-001
traffic_profiles:
  - { id: bg, source: zfl, target: internet, bandwidth_mbps: 0 }
hypothesis:
  id: H1
  statement: A VLAN slice keeps control-traffic p99 within 2x of idle under uplink saturation.
parameters:
  load_mbps: { type: integer, unit: Mbit/s, default: 0, min: 0, max: 1000,
               binds: "traffic_profiles[bg].bandwidth_mbps" }
metrics:
  - { id: ctl_rtt_seconds, kind: histogram, unit: s, entity: measurements/ctl-rtt }
criteria:
  - id: h1-p99-bound
    lhs: { metric: ctl_rtt_seconds, aggregate: p99, select: { arm: saturated } }
    op: le
    rhs: { metric: ctl_rtt_seconds, aggregate: p99, select: { arm: idle }, factor: 2.0 }
    min_samples: 100
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

    fn doc(text: &str) -> Value {
        serde_yaml_ng::from_str(text).unwrap()
    }

    fn edited(from: &str, to: &str) -> Value {
        assert!(EXP.contains(from), "{from}");
        doc(&EXP.replacen(from, to, 1))
    }

    fn fields(problems: &[Problem]) -> Vec<(ProblemKind, String)> {
        problems.iter().map(|p| (p.kind, p.field.clone())).collect()
    }

    #[test]
    fn the_spec_example_is_clean_and_an_experiment_without_intent_has_nothing_to_check() {
        let (intent, problems) = read(&doc(EXP));
        assert_eq!(problems, Vec::new());
        assert_eq!(intent.hypothesis.unwrap().id, "H1");
        assert_eq!(intent.campaign.unwrap().arms.len(), 2);
        assert_eq!(
            check(&doc(
                "kind: experiment\nid: plain\nmethod: { repetitions: 3 }\n"
            )),
            Vec::new()
        );
    }

    #[test]
    fn binds_resolves_by_id_path_and_refuses_what_it_cannot() {
        let d = doc(EXP);
        assert_eq!(
            resolve(&d, "traffic_profiles[bg].bandwidth_mbps")
                .unwrap()
                .as_u64(),
            Some(0)
        );
        assert!(
            resolve(&d, "traffic_profiles[nope].bandwidth_mbps")
                .unwrap_err()
                .contains("no element has `id: nope`")
        );
        assert!(
            resolve(&d, "traffic_profiles[bg].bandwidth")
                .unwrap_err()
                .contains("does not exist")
        );
        assert!(resolve(&d, "traffic_profiles[").is_err());

        let unresolved = check(&edited(
            "binds: \"traffic_profiles[bg].bandwidth_mbps\"",
            "binds: \"traffic_profiles[fg].bandwidth_mbps\"",
        ));
        assert_eq!(
            fields(&unresolved),
            [(ProblemKind::Parameter, "parameters.load_mbps.binds".into())]
        );
        let mistyped = check(&edited("bandwidth_mbps: 0 }", "bandwidth_mbps: fast }"));
        assert!(mistyped[0].detail.contains("is a string"), "{mistyped:?}");
    }

    #[test]
    fn two_parameters_may_not_bind_one_field() {
        let twice = edited(
            "metrics:",
            "  rate: { type: integer, unit: Mbit/s, default: 0, binds: \"traffic_profiles[bg].bandwidth_mbps\" }\nmetrics:",
        );
        let problems = check(&twice);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].detail.contains("already bound by parameter"),
            "{problems:?}"
        );
    }

    #[test]
    fn parameter_problems_name_the_field() {
        let cases = [
            (
                "default: 0, min: 0",
                "default: -5, min: 0",
                "parameters.load_mbps.default",
            ),
            (
                "unit: Mbit/s, default: 0, min",
                "default: 0, min",
                "parameters.load_mbps.unit",
            ),
            (
                "saturated: { load_mbps: 600 }",
                "saturated: { load_mbps: 6000 }",
                "campaign.arms.saturated.load_mbps",
            ),
            (
                "saturated: { load_mbps: 600 }",
                "saturated: { load_gbps: 1 }",
                "campaign.arms.saturated.load_gbps",
            ),
            (
                "[0, 100, 250, 400, 600]",
                "[0, 100, fast]",
                "campaign.sweep.load_mbps[2]",
            ),
            (
                "repetitions: 5\n",
                "repetitions: 5\n  rounds: 2\n",
                "campaign",
            ),
        ];
        for (from, to, field) in cases {
            let problems = check(&edited(from, to));
            assert_eq!(
                fields(&problems),
                [(ProblemKind::Parameter, field.to_string())],
                "{to}: {problems:?}"
            );
        }
    }

    #[test]
    fn campaign_replaces_method_and_an_experiment_declares_one() {
        let both = edited("campaign:", "method: { repetitions: 3 }\ncampaign:");
        assert_eq!(
            fields(&check(&both)),
            [(ProblemKind::Parameter, "campaign".into())]
        );
    }

    #[test]
    fn criterion_problems_name_the_field() {
        let cases = [
            (
                "metric: ctl_rtt_seconds, aggregate: p99, select: { arm: saturated }",
                "metric: rtt, aggregate: p99, select: { arm: saturated }",
                "criteria[0].lhs.metric",
            ),
            (
                "aggregate: p99, select: { arm: saturated }",
                "aggregate: last, select: { arm: saturated }",
                "criteria[0].lhs.aggregate",
            ),
            (
                "select: { arm: saturated }",
                "select: { arm: overload }",
                "criteria[0].lhs.select.arm",
            ),
            (
                "select: { arm: saturated }",
                "select: { load_mbps: 5000 }",
                "criteria[0].lhs.select.load_mbps",
            ),
            (
                "select: { arm: idle }, factor: 2.0 }",
                "select: { arm: idle }, factor: 2.0 }\n    tolerance: 1.0",
                "criteria[0].tolerance",
            ),
            ("op: le", "op: within", "criteria[0].tolerance"),
        ];
        for (from, to, field) in cases {
            let problems = check(&edited(from, to));
            assert_eq!(
                fields(&problems),
                [(ProblemKind::Criterion, field.to_string())],
                "{to}: {problems:?}"
            );
        }
        let units = check(&edited(
            "rhs: { metric: ctl_rtt_seconds, aggregate: p99, select: { arm: idle }, factor: 2.0 }",
            "rhs: { value: 5, unit: ms }",
        ));
        assert!(units[0].detail.contains("units must agree"), "{units:?}");
    }

    struct Table(BTreeMap<(String, String), Samples>);

    impl SampleSource for Table {
        fn samples(&self, metric: &str, select: &BTreeMap<String, Value>) -> Option<Samples> {
            let arm = select
                .get("arm")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            self.0.get(&(metric.to_string(), arm)).cloned()
        }
    }

    fn table(idle: &[f64], saturated: &[f64], complete: bool) -> Table {
        let s = |v: &[f64]| Samples {
            values: v.to_vec(),
            complete,
        };
        Table(BTreeMap::from([
            (("ctl_rtt_seconds".into(), "idle".into()), s(idle)),
            (("ctl_rtt_seconds".into(), "saturated".into()), s(saturated)),
        ]))
    }

    /// The criteria table: each row is samples and the outcome they must give. `inconclusive`
    /// is never a pass.
    #[test]
    fn criteria_evaluate_to_pass_fail_or_inconclusive() {
        let (intent, _) = read(&doc(EXP));
        let h1 = &intent.criteria[0];
        let hundred = |x: f64| vec![x; 100];
        let rows: [(&str, Table, Outcome); 5] = [
            (
                "within 2x",
                table(&hundred(0.001), &hundred(0.0019), true),
                Outcome::Pass,
            ),
            (
                "above 2x",
                table(&hundred(0.001), &hundred(0.0021), true),
                Outcome::Fail,
            ),
            (
                "too few samples",
                table(&[0.001; 50], &hundred(0.0019), true),
                Outcome::Inconclusive,
            ),
            (
                "incomplete telemetry",
                table(&hundred(0.001), &hundred(0.0019), false),
                Outcome::Inconclusive,
            ),
            ("no samples", Table(BTreeMap::new()), Outcome::Inconclusive),
        ];
        for (name, src, want) in rows {
            assert_eq!(evaluate(h1, &src), want, "{name}");
        }
    }

    #[test]
    fn a_group_is_decided_only_by_conclusive_members() {
        let pass = Criterion::Comparison(Comparison {
            id: "a".into(),
            op: Op::Le,
            lhs: Operand::Constant(Constant {
                value: 1.0,
                unit: "s".into(),
            }),
            rhs: Operand::Constant(Constant {
                value: 2.0,
                unit: "s".into(),
            }),
            tolerance: None,
            min_samples: None,
        });
        let fail = Criterion::Comparison(Comparison {
            id: "b".into(),
            op: Op::Gt,
            ..match &pass {
                Criterion::Comparison(c) => c.clone(),
                _ => unreachable!(),
            }
        });
        let unknown = Criterion::Comparison(Comparison {
            id: "c".into(),
            op: Op::Le,
            lhs: Operand::Metric(MetricOperand {
                metric: "m".into(),
                aggregate: "mean".into(),
                select: BTreeMap::new(),
                factor: None,
            }),
            rhs: Operand::Constant(Constant {
                value: 2.0,
                unit: "s".into(),
            }),
            tolerance: None,
            min_samples: None,
        });
        let none = Table(BTreeMap::new());
        let group = |all: bool, m: Vec<Criterion>| {
            Criterion::Group(Group {
                id: None,
                all: all.then(|| m.clone()),
                any: (!all).then_some(m),
            })
        };
        assert_eq!(
            evaluate(&group(true, vec![pass.clone(), unknown.clone()]), &none),
            Outcome::Inconclusive
        );
        assert_eq!(
            evaluate(&group(true, vec![fail.clone(), unknown.clone()]), &none),
            Outcome::Fail
        );
        assert_eq!(
            evaluate(&group(false, vec![pass.clone(), unknown.clone()]), &none),
            Outcome::Pass
        );
        assert_eq!(
            evaluate(&group(false, vec![fail.clone(), unknown]), &none),
            Outcome::Inconclusive
        );
        assert_eq!(
            evaluate(&group(true, vec![pass.clone(), pass]), &none),
            Outcome::Pass
        );
        assert_eq!(
            evaluate(&group(false, vec![fail.clone(), fail]), &none),
            Outcome::Fail
        );
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(aggregate(&v, "p99"), Some(99.0));
        assert_eq!(aggregate(&v, "p50"), Some(50.0));
        assert_eq!(aggregate(&v, "max"), Some(100.0));
        assert_eq!(aggregate(&[], "mean"), None);
    }
}
