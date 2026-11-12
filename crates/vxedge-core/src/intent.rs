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

use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

/// Which kind of problem: a parameter (including the campaign that sets parameters), a
/// criterion, a measurement ([`crate::measurements`]) or a scenario or road
/// ([`crate::scenario`]). vxAPI reports them as `ERROR_REASON_PARAMETER_INVALID`,
/// `ERROR_REASON_CRITERION_INVALID`, `ERROR_REASON_MEASUREMENT_INVALID` and
/// `ERROR_REASON_SCENARIO_INVALID`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemKind {
    Parameter,
    Criterion,
    Measurement,
    Scenario,
    /// A `checks` block that cannot run: a tool not pinned, a step out of order or on a node that
    /// is not a container node, a case reading a step that records nothing.
    Check,
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

/// The aggregates a criterion takes over the values of the trials it selects, one per trial
/// (decision X-24), whatever the metric's kind. Percentiles are nearest-rank; `median` averages
/// the middle two of an even count; `iqr` is the spread of the middle half (the range, below four
/// values); `stddev` is the sample standard deviation.
pub const AGGREGATES: &[&str] = &[
    "mean", "median", "min", "max", "sum", "count", "iqr", "stddev", "p50", "p90", "p95", "p99",
];

/// A metric: one value per trial, read from its records. A metric names a `series` (and the
/// `labels` that pick one of its lines), whose value in a trial is its last sample on `entity`
/// inside the trial's measure phase, or inside the measure `window` it names when the campaign
/// declares `windows`; or it is the `ratio` of two such metrics in the same trial, times `factor`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    pub id: String,
    pub kind: MetricKind,
    pub unit: String,
    pub entity: String,
    #[serde(default)]
    pub series: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub ratio: Option<Vec<String>>,
    #[serde(default)]
    pub factor: Option<f64>,
    #[serde(default)]
    pub window: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Within,
    Outside,
}

/// One side of a criterion: an aggregate of a metric's values over the trials it selects, a
/// constant, or a combination of operands.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Operand {
    Metric(MetricOperand),
    Constant(Constant),
    Combined(Combined),
}

/// `ratio: [a, b]` is a / b, `diff: [a, b]` is a - b, and `max` and `min` take the largest or
/// smallest of their operands; the result is then times `factor`, plus `offset`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Combined {
    #[serde(default)]
    pub ratio: Option<Vec<Operand>>,
    #[serde(default)]
    pub diff: Option<Vec<Operand>>,
    #[serde(default)]
    pub max: Option<Vec<Operand>>,
    #[serde(default)]
    pub min: Option<Vec<Operand>>,
    #[serde(default)]
    pub factor: Option<f64>,
    #[serde(default)]
    pub offset: Option<f64>,
}

impl Combined {
    /// Its rule and operands, when it names exactly one rule.
    pub fn rule(&self) -> Option<(&'static str, &[Operand])> {
        let rules = [
            ("ratio", &self.ratio),
            ("diff", &self.diff),
            ("max", &self.max),
            ("min", &self.min),
        ];
        let mut named = rules
            .into_iter()
            .filter_map(|(name, ops)| ops.as_deref().map(|o| (name, o)));
        let first = named.next()?;
        named.next().is_none().then_some(first)
    }
}

/// A `within` or `outside` tolerance: a number in the operands' unit, or an operand.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Tolerance {
    Value(f64),
    Operand(Operand),
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
    /// Added after `factor`, in the metric's unit.
    #[serde(default)]
    pub offset: Option<f64>,
    /// Read from this declared reference campaign's trials instead of the campaign judged.
    #[serde(default)]
    pub reference: Option<String>,
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
    /// For `within`, the most the two operands may differ by; for `outside`, what they must
    /// differ by more than.
    #[serde(default)]
    pub tolerance: Option<Tolerance>,
    /// The fewest trial values each metric operand needs.
    #[serde(default)]
    pub min_samples: Option<u64>,
    /// Criteria that must pass for this one to be conclusive, such as a loss bound under a
    /// latency bound: otherwise it is `inconclusive`.
    #[serde(default)]
    pub guards: Vec<Criterion>,
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
    #[serde(default)]
    pub guards: Vec<Criterion>,
}

/// The `checks` cases a criterion holds to (`crate::checks`): it passes when every one passes in
/// every trial.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cases {
    pub id: String,
    pub cases: Vec<String>,
}

/// A criterion: a comparison of two operands, an `all`/`any` group of criteria, or a set of
/// functional check cases.
// A document holds a handful of criteria; boxing would only cost readability.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Criterion {
    Comparison(Comparison),
    Cases(Cases),
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
    /// A shorter or longer `duration_s` for each window it names, by window id. The windows and
    /// what each sets stay the campaign's.
    #[serde(default)]
    pub windows: BTreeMap<String, f64>,
}

/// One measure window of a trial (`campaign.windows`): the measurements run for `duration_s`
/// with the parameters `set` changes for this window only, then stop, before the next window
/// starts them afresh. Only a parameter that binds a measurement's field may be set.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub id: String,
    pub duration_s: f64,
    #[serde(default)]
    pub set: BTreeMap<String, Value>,
}

/// The binding path prefix a window's `set` may change: a measurement's field.
pub const MEASUREMENT_BIND: &str = "measurements[";

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
    pub windows: Vec<Window>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// A campaign another experiment's criteria read (`references`): exp-010's H2 compares its own
/// growth with exp-001's. Its trials are read with this experiment's metrics, as `Compare` reads a
/// reference.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    /// The campaign's id: one this host ran, or a record in a workspace's `results/<campaign>/`.
    pub campaign: String,
    #[serde(default)]
    pub statement: Option<String>,
}

/// An experiment's typed intent, as far as it parses.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Intent {
    pub hypothesis: Option<Hypothesis>,
    pub references: BTreeMap<String, Reference>,
    pub parameters: BTreeMap<String, Parameter>,
    pub metrics: Vec<Metric>,
    pub criteria: Vec<Criterion>,
    pub campaign: Option<Campaign>,
    /// The functional checks a trial runs, and the cases that judge them (`crate::checks`).
    pub checks: Option<crate::checks::Checks>,
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
        references: c
            .section("references", ProblemKind::Criterion)
            .unwrap_or_default(),
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
        checks: c.section("checks", ProblemKind::Check),
    };
    if let Some(k) = &intent.checks {
        c.problems.extend(crate::checks::problems(&c.experiment, k));
        check_checks_campaign(&mut c, &intent);
    }
    check_parameters(&mut c, &intent);
    check_campaign(&mut c, &intent);
    campaign_and_method(&mut c);
    check_metrics(&mut c, &intent);
    c.problems.extend(crate::checks::metric_problems(
        &c.experiment,
        intent.checks.as_ref(),
        &intent.metrics,
    ));
    for (name, r) in &intent.references {
        let mut parts = std::path::Path::new(&r.campaign).components();
        let one = matches!(
            (parts.next(), parts.next()),
            (Some(std::path::Component::Normal(_)), None)
        );
        if !one {
            c.push(
                ProblemKind::Criterion,
                format!("references.{name}.campaign"),
                format!(
                    "`{}` is not a campaign id: it names one folder under `results/`",
                    r.campaign
                ),
            );
        }
    }
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

/// A trial runs its checks in the measure phase, within the measure duration: one phase, so no
/// windows, and a duration to bound them.
fn check_checks_campaign(c: &mut Checker<'_>, intent: &Intent) {
    let windows = intent
        .campaign
        .as_ref()
        .is_some_and(|k| !k.windows.is_empty());
    if windows {
        c.push(
            ProblemKind::Check,
            "checks",
            "a trial runs its checks in one measure phase, so a campaign with `checks` has no \
             `windows`",
        );
    }
    let duration = intent
        .campaign
        .as_ref()
        .and_then(|k| k.duration_s)
        .is_some()
        || c.doc
            .get("method")
            .and_then(|m| m.get("duration_s"))
            .is_some();
    if !windows && !duration {
        c.push(
            ProblemKind::Check,
            "checks",
            "the checks run within the measure duration: give `campaign.duration_s` (or \
             `method.duration_s`), the longest they may take",
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

/// The prefix of a bind into the scenario's storyboard parameters:
/// `scenario.parameters.<ParameterDeclaration name>`.
pub const SCENARIO_BIND: &str = "scenario.parameters.";

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
        // A storyboard parameter lives in the scenario document, which `crate::scenario` checks
        // with the workspace in hand.
        if path.starts_with(SCENARIO_BIND) {
            if let Some(other) = bound.insert(path, name) {
                c.push(
                    ProblemKind::Parameter,
                    field("binds"),
                    format!("`{path}` is already bound by parameter `{other}`"),
                );
            }
            continue;
        }
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
    if campaign.windows.is_empty() {
        for (name, p) in &campaign.profiles {
            if !p.windows.is_empty() {
                c.push(
                    ProblemKind::Parameter,
                    format!("campaign.profiles.{name}.windows"),
                    "the campaign declares no `windows` for a profile to change",
                );
            }
        }
        return;
    }
    if campaign.duration_s.is_some() {
        c.push(
            ProblemKind::Parameter,
            "campaign.duration_s",
            "a campaign with `windows` measures for their sum: give each window its `duration_s`",
        );
    }
    for (name, p) in &campaign.profiles {
        if p.duration_s.is_some() {
            c.push(
                ProblemKind::Parameter,
                format!("campaign.profiles.{name}.duration_s"),
                "a campaign with `windows` measures for their sum: change a window's length with \
                 the profile's `windows`",
            );
        }
        for (id, d) in &p.windows {
            let field = format!("campaign.profiles.{name}.windows.{id}");
            if !campaign.windows.iter().any(|w| &w.id == id) {
                c.push(
                    ProblemKind::Parameter,
                    field,
                    format!("`{id}` is not one of the campaign's `windows`"),
                );
            } else if d.is_nan() || *d <= 0.0 {
                c.push(
                    ProblemKind::Parameter,
                    field,
                    "a window needs a duration above 0",
                );
            }
        }
    }
    let mut ids = BTreeSet::new();
    for (i, w) in campaign.windows.iter().enumerate() {
        let field = |f: &str| format!("campaign.windows[{i}].{f}");
        if w.id.is_empty() || !ids.insert(w.id.as_str()) {
            c.push(
                ProblemKind::Parameter,
                field("id"),
                format!(
                    "`{}` must be a non-empty id, unique among the windows",
                    w.id
                ),
            );
        }
        if w.duration_s.is_nan() || w.duration_s <= 0.0 {
            c.push(
                ProblemKind::Parameter,
                field("duration_s"),
                "a window needs a duration above 0",
            );
        }
        for (param, value) in &w.set {
            let f = format!("campaign.windows[{i}].set.{param}");
            set(c, f.clone(), param, value);
            let binds = intent
                .parameters
                .get(param)
                .and_then(|p| p.binds.as_deref());
            if intent.parameters.contains_key(param)
                && !binds.is_some_and(|b| b.starts_with(MEASUREMENT_BIND))
            {
                c.push(
                    ProblemKind::Parameter,
                    f,
                    format!(
                        "`{param}` binds no measurement field: a window may set only what its \
                         measurements read, since the rest of the trial is already up"
                    ),
                );
            }
        }
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
        match (&m.series, &m.ratio) {
            (Some(s), None) if !s.is_empty() => {}
            (None, Some(members)) => {
                if members.len() != 2 {
                    c.push(
                        ProblemKind::Criterion,
                        format!("metrics[{i}].ratio"),
                        "a ratio names two metrics",
                    );
                }
                for (j, id) in members.iter().enumerate() {
                    let named = intent.metrics.iter().find(|x| &x.id == id);
                    if !named.is_some_and(|x| x.series.is_some()) {
                        c.push(
                            ProblemKind::Criterion,
                            format!("metrics[{i}].ratio[{j}]"),
                            format!("`{id}` is not a declared metric with a `series`"),
                        );
                    }
                }
                if !m.labels.is_empty() {
                    c.push(
                        ProblemKind::Criterion,
                        format!("metrics[{i}].labels"),
                        "`labels` pick a series's line; a ratio has none",
                    );
                }
            }
            _ => c.push(
                ProblemKind::Criterion,
                format!("metrics[{i}]"),
                "a metric names a `series` or a `ratio` of two metrics, so a trial's records give \
                 it a value",
            ),
        }
        if m.factor.is_some() && m.ratio.is_none() {
            c.push(
                ProblemKind::Criterion,
                format!("metrics[{i}].factor"),
                "only a `ratio` takes a `factor`",
            );
        }
        let windows: Vec<&str> = intent
            .campaign
            .iter()
            .flat_map(|c| c.windows.iter().map(|w| w.id.as_str()))
            .collect();
        match (&m.window, m.series.is_some(), windows.is_empty()) {
            (Some(_), false, _) => c.push(
                ProblemKind::Criterion,
                format!("metrics[{i}].window"),
                "a ratio reads its members' windows and names none itself",
            ),
            (Some(w), true, true) => c.push(
                ProblemKind::Criterion,
                format!("metrics[{i}].window"),
                format!("`{w}` is not a window: the campaign declares no `windows`"),
            ),
            (Some(w), true, false) if !windows.contains(&w.as_str()) => c.push(
                ProblemKind::Criterion,
                format!("metrics[{i}].window"),
                format!(
                    "`{w}` is not a window of the campaign; there are {}",
                    windows.join(", ")
                ),
            ),
            (None, true, false) => c.push(
                ProblemKind::Criterion,
                format!("metrics[{i}].window"),
                format!(
                    "the campaign measures in windows ({}): name the one this metric reads",
                    windows.join(", ")
                ),
            ),
            _ => {}
        }
    }
}

/// The unit an operand is in, once checked. A ratio of two operands of one unit is in `1`.
fn check_operand(
    c: &mut Checker<'_>,
    intent: &Intent,
    operand: &Operand,
    field: &str,
) -> Option<String> {
    match operand {
        Operand::Constant(k) => Some(k.unit.clone()),
        Operand::Combined(k) => {
            let Some((rule, members)) = k.rule() else {
                c.push(
                    ProblemKind::Criterion,
                    field,
                    "an operand is a `metric`, a constant `value`, or exactly one of `ratio`, \
                     `diff`, `max` or `min`",
                );
                return None;
            };
            let pair = matches!(rule, "ratio" | "diff");
            if (pair && members.len() != 2) || members.is_empty() {
                c.push(
                    ProblemKind::Criterion,
                    format!("{field}.{rule}"),
                    if pair {
                        format!("`{rule}` takes two operands")
                    } else {
                        format!("`{rule}` takes at least one operand")
                    },
                );
                return None;
            }
            let units: Vec<Option<String>> = members
                .iter()
                .enumerate()
                .map(|(i, m)| check_operand(c, intent, m, &format!("{field}.{rule}[{i}]")))
                .collect();
            let units: Option<Vec<String>> = units.into_iter().collect();
            let units = units?;
            if units.iter().any(|u| u != &units[0]) {
                c.push(
                    ProblemKind::Criterion,
                    format!("{field}.{rule}"),
                    format!(
                        "combines {}; its operands' units must agree",
                        units
                            .iter()
                            .map(|u| format!("`{u}`"))
                            .collect::<Vec<_>>()
                            .join(" with ")
                    ),
                );
                return None;
            }
            Some(if rule == "ratio" {
                "1".to_string()
            } else {
                units[0].clone()
            })
        }
        Operand::Metric(m) => {
            if let Some(r) = &m.reference
                && !intent.references.contains_key(r)
            {
                c.push(
                    ProblemKind::Criterion,
                    format!("{field}.reference"),
                    format!("`{r}` is not a declared reference"),
                );
            }
            let Some(metric) = intent.metrics.iter().find(|x| x.id == m.metric) else {
                c.push(
                    ProblemKind::Criterion,
                    format!("{field}.metric"),
                    format!("`{}` is not a declared metric", m.metric),
                );
                return None;
            };
            if !AGGREGATES.contains(&m.aggregate.as_str()) {
                c.push(
                    ProblemKind::Criterion,
                    format!("{field}.aggregate"),
                    format!(
                        "`{}` is not an aggregate; there are {}",
                        m.aggregate,
                        AGGREGATES.join(", ")
                    ),
                );
            }
            for (key, value) in &m.select {
                let at = format!("{field}.select.{key}");
                if key == "arm" {
                    // A campaign that does not read is reported on its own; its arms are unknown,
                    // not missing. A reference's arms are its own campaign's.
                    if (intent.campaign.is_none() && c.doc.get("campaign").is_some())
                        || m.reference.is_some()
                    {
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
            // `count` counts trials, whatever the metric measures.
            Some(if m.aggregate == "count" {
                "1".to_string()
            } else {
                metric.unit.clone()
            })
        }
    }
}

/// Whether an operand reads no metric at all.
fn constant_only(operand: &Operand) -> bool {
    match operand {
        Operand::Constant(_) => true,
        Operand::Metric(_) => false,
        Operand::Combined(k) => k
            .rule()
            .is_none_or(|(_, members)| members.iter().all(constant_only)),
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
    let guards = match criterion {
        Criterion::Group(g) => &g.guards,
        Criterion::Comparison(k) => &k.guards,
        Criterion::Cases(_) => &Vec::new(),
    };
    match criterion {
        Criterion::Cases(k) => {
            unique(c, &k.id);
            if k.cases.is_empty() {
                c.push(
                    ProblemKind::Criterion,
                    format!("{field}.cases"),
                    "names no case",
                );
            }
            for name in &k.cases {
                let detail = match intent.checks.as_ref().map(|ch| ch.case(name)) {
                    None => Some(format!(
                        "names case `{name}`, and the experiment declares no `checks`"
                    )),
                    Some(None) => {
                        Some(format!("no case `{name}` is declared under `checks.cases`"))
                    }
                    Some(Some(case)) if case.observe => Some(format!(
                        "case `{name}` is an `observe` case: it records, and is never judged"
                    )),
                    Some(Some(_)) => None,
                };
                if let Some(detail) = detail {
                    c.push(ProblemKind::Criterion, format!("{field}.cases"), detail);
                }
            }
        }
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
            let banded = matches!(k.op, Op::Within | Op::Outside);
            match (&k.tolerance, banded) {
                (None, true) => c.push(
                    ProblemKind::Criterion,
                    format!("{field}.tolerance"),
                    "`within` and `outside` need a `tolerance`",
                ),
                (Some(_), false) => c.push(
                    ProblemKind::Criterion,
                    format!("{field}.tolerance"),
                    "only `within` and `outside` take a `tolerance`",
                ),
                (Some(Tolerance::Value(t)), true) if *t < 0.0 => c.push(
                    ProblemKind::Criterion,
                    format!("{field}.tolerance"),
                    "cannot be negative",
                ),
                (Some(Tolerance::Operand(o)), true) => {
                    let at = format!("{field}.tolerance");
                    if let (Some(t), Some(l)) = (check_operand(c, intent, o, &at), &lhs)
                        && &t != l
                    {
                        c.push(
                            ProblemKind::Criterion,
                            at,
                            format!("is in `{t}`, and the operands are in `{l}`"),
                        );
                    }
                }
                _ => {}
            }
            if constant_only(&k.lhs) && constant_only(&k.rhs) {
                c.push(
                    ProblemKind::Criterion,
                    field,
                    "compares two constants; nothing is measured",
                );
            }
        }
    }
    for (i, g) in guards.iter().enumerate() {
        check_criterion(c, intent, g, &format!("{field}.guards[{i}]"), ids);
    }
}

/// What a criterion found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Pass,
    Fail,
    Inconclusive,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::Inconclusive => "inconclusive",
        }
    }
}

/// The values of one metric under one selection, one per trial it selects, and whether every
/// one of those trials gave a value from complete telemetry.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Samples {
    pub values: Vec<f64>,
    /// The trial each value is from, in the same order.
    pub trials: Vec<String>,
    pub complete: bool,
}

/// Where [`judge`] reads values from: archives ([`crate::verdict`]), or a live recording.
pub trait SampleSource {
    /// The values of `metric` under `select`, or `None` when no trial is selected at all.
    fn samples(&self, metric: &str, select: &BTreeMap<String, Value>) -> Option<Samples>;

    /// The same from the trials of the reference campaign `reference`, or why they cannot be read.
    fn reference_samples(
        &self,
        reference: &str,
        _metric: &str,
        _select: &BTreeMap<String, Value>,
    ) -> Result<Option<Samples>, String> {
        Err(format!("reference `{reference}` was not given"))
    }

    /// What `case` found in each trial, in trial order, or `None` when no trial is selected.
    fn case_results(&self, _case: &str) -> Option<Vec<CaseRead>> {
        None
    }
}

/// What one functional check case found in one trial.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaseRead {
    pub case: String,
    pub trial: String,
    #[serde(flatten)]
    pub result: crate::checks::CaseResult,
}

/// `aggregate` of `values`, which are not empty. Percentiles are nearest-rank; `median` and
/// `iqr` are those of Python's `statistics` (`median`, and `quantiles(n=4)` from four values,
/// the range below), so a verdict matches a notebook's.
pub fn aggregate(values: &[f64], aggregate: &str) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let n = values.len();
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = |p: f64| {
        let i = ((p / 100.0) * n as f64).ceil() as usize;
        sorted[i.clamp(1, n) - 1]
    };
    let mean = values.iter().sum::<f64>() / n as f64;
    Some(match aggregate {
        "mean" => mean,
        "median" if n % 2 == 1 => sorted[n / 2],
        "median" => (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0,
        "min" => sorted[0],
        "max" => sorted[n - 1],
        "sum" => values.iter().sum(),
        "count" => n as f64,
        "iqr" if n < 4 => sorted[n - 1] - sorted[0],
        "iqr" => {
            // `statistics.quantiles(method="exclusive")`: positions i(n+1)/4.
            let q = |i: usize| {
                let m = n + 1;
                let j = (i * m / 4).clamp(1, n - 1);
                let delta = (i * m - j * 4) as f64;
                (sorted[j - 1] * (4.0 - delta) + sorted[j] * delta) / 4.0
            };
            q(3) - q(1)
        }
        "stddev" if n < 2 => return None,
        "stddev" => {
            let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
            var.sqrt()
        }
        "p50" => rank(50.0),
        "p90" => rank(90.0),
        "p95" => rank(95.0),
        "p99" => rank(99.0),
        _ => return None,
    })
}

/// One metric a criterion read: under which selection, the trials and their values, and the
/// aggregate it took.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Read {
    pub metric: String,
    /// The reference campaign it was read from, when not the campaign judged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    pub aggregate: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub select: BTreeMap<String, Value>,
    pub trials: Vec<String>,
    pub values: Vec<f64>,
    pub complete: bool,
    pub value: Option<f64>,
}

/// How a criterion was judged: its outcome, the operands' values, every metric it read, its
/// members and guards, and why it is `inconclusive` when it is.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Judgement {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub outcome: Outcome,
    /// `lt` … `outside` for a comparison, `all` or `any` for a group.
    pub rule: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lhs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rhs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<Read>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<Judgement>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub guards: Vec<Judgement>,
    /// For a `cases` criterion: each case in each trial.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cases: Vec<CaseRead>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

fn op_name(op: Op) -> &'static str {
    match op {
        Op::Lt => "lt",
        Op::Le => "le",
        Op::Gt => "gt",
        Op::Ge => "ge",
        Op::Within => "within",
        Op::Outside => "outside",
    }
}

/// An operand's value, or why it has none. Every metric it reads is added to `reads`.
fn operand_value(
    operand: &Operand,
    min_samples: u64,
    src: &dyn SampleSource,
    reads: &mut Vec<Read>,
) -> Result<f64, String> {
    match operand {
        Operand::Constant(k) => Ok(k.value),
        Operand::Metric(m) => {
            let (s, unavailable) = match &m.reference {
                None => (src.samples(&m.metric, &m.select).unwrap_or_default(), None),
                Some(r) => match src.reference_samples(r, &m.metric, &m.select) {
                    Ok(s) => (s.unwrap_or_default(), None),
                    Err(why) => (Samples::default(), Some(why)),
                },
            };
            let value = aggregate(&s.values, &m.aggregate)
                .map(|v| v * m.factor.unwrap_or(1.0) + m.offset.unwrap_or(0.0));
            reads.push(Read {
                metric: m.metric.clone(),
                reference: m.reference.clone(),
                aggregate: m.aggregate.clone(),
                select: m.select.clone(),
                trials: s.trials.clone(),
                values: s.values.clone(),
                complete: s.complete,
                value,
            });
            let n = s.values.len() as u64;
            let from = m
                .reference
                .as_ref()
                .map(|r| format!(" in reference `{r}`"))
                .unwrap_or_default();
            if let Some(why) = unavailable {
                Err(why)
            } else if n == 0 {
                Err(format!("no trial{from} gives `{}` a value", m.metric))
            } else if !s.complete {
                Err(format!("`{}`'s telemetry is incomplete", m.metric))
            } else if n < min_samples.max(1) {
                Err(format!(
                    "`{}` has {n} trial values, fewer than {min_samples}",
                    m.metric
                ))
            } else {
                value.ok_or_else(|| format!("`{}` has no `{}`", m.metric, m.aggregate))
            }
        }
        Operand::Combined(k) => {
            let (rule, members) = k.rule().ok_or("an operand names no rule")?;
            // Every member is read, so the report shows them all.
            let values: Vec<Result<f64, String>> = members
                .iter()
                .map(|m| operand_value(m, min_samples, src, reads))
                .collect();
            let values: Vec<f64> = values.into_iter().collect::<Result<_, _>>()?;
            let v = match (rule, values.as_slice()) {
                ("ratio", [_, b]) if *b == 0.0 => return Err("a ratio divides by 0".into()),
                ("ratio", [a, b]) => a / b,
                ("diff", [a, b]) => a - b,
                ("max", vs) if !vs.is_empty() => vs.iter().copied().fold(f64::MIN, f64::max),
                ("min", vs) if !vs.is_empty() => vs.iter().copied().fold(f64::MAX, f64::min),
                _ => return Err(format!("`{rule}` has the wrong number of operands")),
            };
            Ok(v * k.factor.unwrap_or(1.0) + k.offset.unwrap_or(0.0))
        }
    }
}

/// Judge `criterion` over `src`. A comparison is `inconclusive` when an operand has no values,
/// fewer than `min_samples`, or incomplete telemetry. `all` fails on any conclusive failure and
/// `any` passes on any conclusive pass; otherwise a group that has an inconclusive member is
/// inconclusive. A criterion whose guards do not all pass is `inconclusive`, whatever it found.
/// `inconclusive` is never turned into a pass.
pub fn judge(criterion: &Criterion, src: &dyn SampleSource) -> Judgement {
    let (guards, mut j) = match criterion {
        Criterion::Comparison(k) => (&k.guards, judge_comparison(k, src)),
        Criterion::Group(g) => (&g.guards, judge_group(g, src)),
        Criterion::Cases(k) => (&Vec::new(), judge_cases(k, src)),
    };
    j.guards = guards.iter().map(|g| judge(g, src)).collect();
    if let Some(g) = j.guards.iter().find(|g| g.outcome != Outcome::Pass) {
        j.why = Some(format!(
            "guard `{}` is {}",
            g.id.as_deref().unwrap_or(&g.rule),
            g.outcome.as_str()
        ));
        j.outcome = Outcome::Inconclusive;
    }
    j
}

fn judge_comparison(k: &Comparison, src: &dyn SampleSource) -> Judgement {
    let n = k.min_samples.unwrap_or(1);
    let mut reads = Vec::new();
    let l = operand_value(&k.lhs, n, src, &mut reads);
    let r = operand_value(&k.rhs, n, src, &mut reads);
    let t = match &k.tolerance {
        None => Ok(0.0),
        Some(Tolerance::Value(v)) => Ok(*v),
        Some(Tolerance::Operand(o)) => operand_value(o, n, src, &mut reads),
    };
    let mut j = Judgement {
        id: Some(k.id.clone()),
        outcome: Outcome::Inconclusive,
        rule: op_name(k.op).into(),
        lhs: l.as_ref().ok().copied(),
        rhs: r.as_ref().ok().copied(),
        tolerance: k.tolerance.as_ref().and(t.as_ref().ok().copied()),
        reads,
        members: Vec::new(),
        guards: Vec::new(),
        cases: Vec::new(),
        why: None,
    };
    match (l, r, t) {
        (Ok(l), Ok(r), Ok(t)) => {
            let holds = match k.op {
                Op::Lt => l < r,
                Op::Le => l <= r,
                Op::Gt => l > r,
                Op::Ge => l >= r,
                Op::Within => (l - r).abs() <= t,
                Op::Outside => (l - r).abs() > t,
            };
            j.outcome = if holds { Outcome::Pass } else { Outcome::Fail };
        }
        (Err(why), _, _) | (_, Err(why), _) | (_, _, Err(why)) => j.why = Some(why),
    }
    j
}

/// A `cases` criterion fails when a case fails in any trial, and is `inconclusive` when none
/// fails but one could not be judged, or no trial ran.
fn judge_cases(k: &Cases, src: &dyn SampleSource) -> Judgement {
    let mut cases = Vec::new();
    let mut why = None;
    for name in &k.cases {
        match src.case_results(name) {
            Some(reads) if !reads.is_empty() => cases.extend(reads),
            _ => {
                why.get_or_insert_with(|| format!("no trial records case `{name}`"));
            }
        }
    }
    let has = |o| cases.iter().any(|r| r.result.outcome == Some(o));
    let outcome = if has(Outcome::Fail) {
        Outcome::Fail
    } else if why.is_some() || has(Outcome::Inconclusive) || cases.is_empty() {
        Outcome::Inconclusive
    } else {
        Outcome::Pass
    };
    if outcome == Outcome::Fail {
        why = cases
            .iter()
            .find(|r| r.result.outcome == Some(Outcome::Fail))
            .map(|r| {
                format!(
                    "case `{}` in trial `{}`: {}",
                    r.case,
                    r.trial,
                    r.result.why.as_deref().unwrap_or("failed")
                )
            });
    } else if outcome == Outcome::Inconclusive && why.is_none() {
        why = cases
            .iter()
            .find(|r| r.result.outcome == Some(Outcome::Inconclusive))
            .map(|r| {
                format!(
                    "case `{}` in trial `{}`: {}",
                    r.case,
                    r.trial,
                    r.result.why.as_deref().unwrap_or("inconclusive")
                )
            });
    }
    Judgement {
        id: Some(k.id.clone()),
        outcome,
        rule: "cases".into(),
        lhs: None,
        rhs: None,
        tolerance: None,
        reads: Vec::new(),
        members: Vec::new(),
        guards: Vec::new(),
        cases,
        why,
    }
}

fn judge_group(g: &Group, src: &dyn SampleSource) -> Judgement {
    let (members, all) = match (&g.all, &g.any) {
        (Some(m), _) => (m, true),
        (None, Some(m)) => (m, false),
        (None, None) => (&Vec::new(), true),
    };
    let members: Vec<Judgement> = members.iter().map(|m| judge(m, src)).collect();
    let decisive = if all { Outcome::Fail } else { Outcome::Pass };
    let has = |o| members.iter().any(|m| m.outcome == o);
    let outcome = if has(decisive) {
        decisive
    } else if has(Outcome::Inconclusive) || members.is_empty() {
        Outcome::Inconclusive
    } else if all {
        Outcome::Pass
    } else {
        Outcome::Fail
    };
    Judgement {
        id: g.id.clone(),
        outcome,
        rule: if all { "all" } else { "any" }.into(),
        lhs: None,
        rhs: None,
        tolerance: None,
        reads: Vec::new(),
        why: (outcome == Outcome::Inconclusive)
            .then(|| "an inconclusive member decides it".to_string()),
        members,
        guards: Vec::new(),
        cases: Vec::new(),
    }
}

/// [`judge`]'s outcome alone.
pub fn evaluate(criterion: &Criterion, src: &dyn SampleSource) -> Outcome {
    judge(criterion, src).outcome
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
  - { id: ctl_rtt_seconds, kind: gauge, unit: s, entity: measurements/ctl-rtt,
      series: vxprobe_rtt_seconds, labels: { quantile: "0.99" } }
criteria:
  - id: h1-p99-bound
    lhs: { metric: ctl_rtt_seconds, aggregate: median, select: { arm: saturated } }
    op: le
    rhs: { metric: ctl_rtt_seconds, aggregate: median, select: { arm: idle }, factor: 2.0 }
    min_samples: 5
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

    /// A bind into the scenario's storyboard parameters is not resolved in the experiment
    /// document; `crate::scenario` checks it.
    #[test]
    fn a_scenario_bind_is_left_to_the_scenario_check() {
        let d = edited(
            "binds: \"traffic_profiles[bg].bandwidth_mbps\"",
            "binds: \"scenario.parameters.EgoTargetSpeed\"",
        );
        assert_eq!(check(&d), Vec::new());
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
                "metric: ctl_rtt_seconds, aggregate: median, select: { arm: saturated }",
                "metric: rtt, aggregate: median, select: { arm: saturated }",
                "criteria[0].lhs.metric",
            ),
            (
                "aggregate: median, select: { arm: saturated }",
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
            ("op: le", "op: outside", "criteria[0].tolerance"),
            (
                "factor: 2.0 }",
                "factor: 2.0, offset: 0.001 }\n    guards: [{ id: h1-p99-bound, op: gt, lhs: { value: 1, unit: \"1\" }, rhs: { metric: ctl_rtt_seconds, aggregate: count } }]",
                "criteria[0].guards[0].id",
            ),
            ("series: vxprobe_rtt_seconds, ", "", "metrics[0]"),
            (
                "rhs: { metric: ctl_rtt_seconds, aggregate: median, select: { arm: idle }, factor: 2.0 }",
                "rhs: { diff: [{ metric: ctl_rtt_seconds, aggregate: median }] }",
                "criteria[0].rhs.diff",
            ),
            (
                "rhs: { metric: ctl_rtt_seconds, aggregate: median, select: { arm: idle }, factor: 2.0 }",
                "rhs: { max: [{ metric: ctl_rtt_seconds, aggregate: median }], min: [{ value: 1, unit: s }] }",
                "criteria[0].rhs",
            ),
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
            "rhs: { metric: ctl_rtt_seconds, aggregate: median, select: { arm: idle }, factor: 2.0 }",
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
            trials: (0..v.len()).map(|i| format!("r{i}")).collect(),
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
        let hundred = |x: f64| vec![x; 5];
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
                table(&[0.001; 3], &hundred(0.0019), true),
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
            guards: vec![],
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
                offset: None,
                reference: None,
            }),
            rhs: Operand::Constant(Constant {
                value: 2.0,
                unit: "s".into(),
            }),
            tolerance: None,
            min_samples: None,
            guards: vec![],
        });
        let none = Table(BTreeMap::new());
        let group = |all: bool, m: Vec<Criterion>| {
            Criterion::Group(Group {
                id: None,
                all: all.then(|| m.clone()),
                any: (!all).then_some(m),
                guards: vec![],
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

    /// `median`, `iqr` and `stddev` equal Python's `statistics` (`median`, `quantiles(n=4)`,
    /// `stdev`), as the notebooks compute them; `iqr` below four values is the range.
    #[test]
    fn median_iqr_and_stddev_match_the_notebooks() {
        let cases: [(&[f64], f64, f64, f64); 4] = [
            (&[1.0, 2.0, 3.0, 4.0, 5.0], 3.0, 3.0, 1.5811388300841898),
            (&[1.0, 2.0, 4.0, 8.0], 3.0, 5.75, 3.095695936834452),
            (
                &[5.0, 1.0, 9.0, 2.0, 7.0, 3.0],
                4.0,
                5.75,
                3.082207001484488,
            ),
            (&[3.0, 1.0, 2.0], 2.0, 2.0, 1.0),
        ];
        for (v, median, iqr, stddev) in cases {
            assert_eq!(aggregate(v, "median"), Some(median), "{v:?}");
            assert_eq!(aggregate(v, "iqr"), Some(iqr), "{v:?}");
            assert!(
                (aggregate(v, "stddev").unwrap() - stddev).abs() < 1e-12,
                "{v:?}"
            );
        }
        let twenty: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(aggregate(&twenty, "median"), Some(10.5));
        assert_eq!(aggregate(&twenty, "iqr"), Some(10.5));
        assert_eq!(aggregate(&[1.0], "stddev"), None);
    }

    struct Fixed(BTreeMap<String, Vec<f64>>);

    impl SampleSource for Fixed {
        fn samples(&self, metric: &str, select: &BTreeMap<String, Value>) -> Option<Samples> {
            let arm = select.get("arm").and_then(Value::as_str).unwrap_or("");
            let values = self.0.get(&format!("{metric}/{arm}"))?.clone();
            Some(Samples {
                trials: (0..values.len()).map(|i| format!("r{i}")).collect(),
                values,
                complete: true,
            })
        }
    }

    fn criterion(yaml: &str) -> Criterion {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    /// The notebooks' rules, written as criteria: an offset bound, a difference beyond the
    /// larger of an IQR and a floor, a ratio of ratios, and a guard that makes a criterion
    /// inconclusive.
    #[test]
    fn combined_operands_offsets_outside_and_guards() {
        let src = Fixed(BTreeMap::from([
            ("loss/idle".into(), vec![0.0, 0.1, 0.2]),
            ("loss/sat".into(), vec![0.5, 0.6, 0.7]),
            ("p50/a".into(), vec![10.0, 11.0, 12.0, 13.0, 14.0]),
            ("p50/b".into(), vec![20.0, 21.0, 22.0, 23.0, 24.0]),
            ("p99/v0".into(), vec![1.0]),
            ("p99/v6".into(), vec![2.0]),
            ("p99/s0".into(), vec![1.0]),
            ("p99/s6".into(), vec![2.1]),
        ]));
        let loss = criterion(
            "{ id: loss, op: le, lhs: { metric: loss, aggregate: median, select: { arm: sat } },
               rhs: { metric: loss, aggregate: median, select: { arm: idle }, offset: 0.5 } }",
        );
        assert_eq!(evaluate(&loss, &src), Outcome::Pass);
        let differs = criterion(
            "{ id: d, op: outside, lhs: { metric: p50, aggregate: median, select: { arm: b } },
               rhs: { metric: p50, aggregate: median, select: { arm: a } },
               tolerance: { max: [{ metric: p50, aggregate: iqr, select: { arm: a } },
                                  { value: 1, unit: us }] } }",
        );
        let j = judge(&differs, &src);
        assert_eq!((j.outcome, j.tolerance), (Outcome::Pass, Some(3.0)));
        let h2 = criterion(
            "{ id: h2, op: le,
               lhs: { ratio: [{ metric: p99, aggregate: median, select: { arm: s6 } },
                              { metric: p99, aggregate: median, select: { arm: s0 } }] },
               rhs: { ratio: [{ metric: p99, aggregate: median, select: { arm: v6 } },
                              { metric: p99, aggregate: median, select: { arm: v0 } }],
                      factor: 1.1 } }",
        );
        assert_eq!(evaluate(&h2, &src), Outcome::Pass);
        let guarded = criterion(
            "{ id: g, op: gt, lhs: { metric: p50, aggregate: median, select: { arm: b } },
               rhs: { value: 0, unit: us },
               guards: [{ id: loss-ok, op: le, lhs: { metric: loss, aggregate: max, select: { arm: sat } },
                          rhs: { value: 0.6, unit: '%' } }] }",
        );
        let j = judge(&guarded, &src);
        assert_eq!(j.outcome, Outcome::Inconclusive);
        assert_eq!(j.why.as_deref(), Some("guard `loss-ok` is fail"));
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(aggregate(&v, "p99"), Some(99.0));
        assert_eq!(aggregate(&v, "p50"), Some(50.0));
        assert_eq!(aggregate(&v, "max"), Some(100.0));
        assert_eq!(aggregate(&[], "mean"), None);
    }

    const WINDOWS: &str = r#"
kind: experiment
id: exp-005
measurements:
  probe: { kind: rtt-probe, from: zfl, to: internet, slice: adas, rate_pps: 1000 }
traffic_profiles:
  - { id: bg, source: zfl, target: internet, bandwidth_mbps: 0 }
parameters:
  probe_pps: { type: integer, unit: 1/s, default: 1000, min: 10, max: 5000,
               binds: "measurements[probe].rate_pps" }
  load_mbps: { type: integer, unit: Mbit/s, default: 0, min: 0, max: 1000,
               binds: "traffic_profiles[bg].bandwidth_mbps" }
metrics:
  - { id: cold_max, kind: gauge, unit: s, entity: measurements/probe,
      series: vxprobe_rtt_seconds_max, window: cold }
  - { id: steady_p50, kind: gauge, unit: s, entity: measurements/probe,
      series: vxprobe_rtt_seconds, labels: { quantile: "0.5" }, window: steady }
  - { id: cold_ratio, kind: gauge, unit: "1", entity: measurements/probe,
      ratio: [cold_max, steady_p50] }
campaign:
  repetitions: 20
  windows:
    - { id: cold, duration_s: 3, set: { probe_pps: 20 } }
    - { id: steady, duration_s: 60 }
  profiles:
    quick: { repetitions: 2 }
"#;

    #[test]
    fn measure_windows_name_their_metrics_and_set_only_measurement_parameters() {
        let (intent, problems) = read(&doc(WINDOWS));
        assert_eq!(problems, Vec::new());
        let windows = &intent.campaign.unwrap().windows;
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].set.get("probe_pps"), Some(&Value::from(20)));

        let cases = [
            (
                "    - { id: steady, duration_s: 60 }",
                "    - { id: steady, duration_s: 60 }\n  duration_s: 63",
                "campaign.duration_s",
            ),
            (
                "    quick: { repetitions: 2 }",
                "    quick: { repetitions: 2, duration_s: 5 }",
                "campaign.profiles.quick.duration_s",
            ),
            (
                "    quick: { repetitions: 2 }",
                "    quick: { repetitions: 2, windows: { warm: 5 } }",
                "campaign.profiles.quick.windows.warm",
            ),
            (
                "    quick: { repetitions: 2 }",
                "    quick: { repetitions: 2, windows: { steady: 0 } }",
                "campaign.profiles.quick.windows.steady",
            ),
            (
                "{ id: steady, duration_s: 60 }",
                "{ id: cold, duration_s: 60 }",
                "campaign.windows[1].id",
            ),
            (
                "{ id: steady, duration_s: 60 }",
                "{ id: steady, duration_s: 0 }",
                "campaign.windows[1].duration_s",
            ),
            (
                "set: { probe_pps: 20 }",
                "set: { load_mbps: 20 }",
                "campaign.windows[0].set.load_mbps",
            ),
            (
                "set: { probe_pps: 20 }",
                "set: { probe_pps: 1 }",
                "campaign.windows[0].set.probe_pps",
            ),
            (
                "set: { probe_pps: 20 }",
                "set: { nope: 1 }",
                "campaign.windows[0].set.nope",
            ),
            (
                "series: vxprobe_rtt_seconds_max, window: cold",
                "series: vxprobe_rtt_seconds_max",
                "metrics[0].window",
            ),
            (
                "series: vxprobe_rtt_seconds_max, window: cold",
                "series: vxprobe_rtt_seconds_max, window: warm",
                "metrics[0].window",
            ),
            (
                "ratio: [cold_max, steady_p50] }",
                "ratio: [cold_max, steady_p50], window: cold }",
                "metrics[2].window",
            ),
        ];
        for (from, to, field) in cases {
            assert!(WINDOWS.contains(from), "{from}");
            let (_, problems) = read(&doc(&WINDOWS.replacen(from, to, 1)));
            assert!(
                problems.iter().any(|p| p.field == field),
                "{to}: {problems:?}"
            );
        }
        // Without windows, a metric names none.
        let (_, problems) = read(&edited(
            "labels: { quantile: \"0.99\" } }",
            "labels: { quantile: \"0.99\" }, window: cold }",
        ));
        assert_eq!(
            fields(&problems),
            [(ProblemKind::Criterion, "metrics[0].window".into())]
        );
    }

    #[test]
    fn a_reference_is_declared_and_names_one_campaign() {
        let with = |refs: &str, operand: &str| {
            EXP.replacen("hypothesis:", &format!("{refs}hypothesis:"), 1).replacen(
                "rhs: { metric: ctl_rtt_seconds, aggregate: median, select: { arm: idle }, factor: 2.0 }",
                operand,
                1,
            )
        };
        let declared = "references:\n  vlan: { campaign: exp-001 }\n";
        let good = with(
            declared,
            "rhs: { metric: ctl_rtt_seconds, aggregate: median, select: { arm: saturated-ref }, reference: vlan }",
        );
        let (intent, problems) = read(&doc(&good));
        assert_eq!(
            problems,
            Vec::new(),
            "a reference's arms are its own campaign's"
        );
        assert_eq!(intent.references["vlan"].campaign, "exp-001");

        let undeclared = with(
            "",
            "rhs: { metric: ctl_rtt_seconds, aggregate: median, reference: vlan }",
        );
        assert_eq!(
            fields(&read(&doc(&undeclared)).1),
            [(ProblemKind::Criterion, "criteria[0].rhs.reference".into())]
        );
        let path = with(
            "references:\n  vlan: { campaign: ../exp-001 }\n",
            "rhs: { metric: ctl_rtt_seconds, aggregate: median, reference: vlan }",
        );
        assert_eq!(
            fields(&read(&doc(&path)).1),
            [(ProblemKind::Criterion, "references.vlan.campaign".into())]
        );
    }
}
