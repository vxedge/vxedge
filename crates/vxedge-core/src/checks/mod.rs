//! **Functional checks** (`ARCHITECTURE.md` §6.0.3): an experiment that runs tools against the
//! system under test and judges what they print, as exp-015 and exp-017 judge a Kuksa databroker
//! with a `kuksa.val.v2` client.
//!
//! A `checks` block declares the `tools` (OCI images, pinned by digest or from this host's
//! `localhost/` store), the `steps` a trial's measure phase runs in order, and the `cases` that
//! judge each step's output. A step runs a tool in a container node's network namespace, in a
//! namespace of its own (`on: alone`), or in a background step's (`with`); or it observes a
//! container's state or a bridge port's counters; or it waits for, stops or pauses on a
//! background step. Each step's output, one line per event, is a log stream of the entity
//! `checks/<step>`, so it lands in the trial's archive with everything else.
//!
//! A case selects the JSON lines of one step (`where`) and expects something of them: a field's
//! value, a count, the sequence of a field's values, or a comparison with another step's. A
//! criterion `{ id, cases: [...] }` passes when every case it names passes in every trial it
//! selects. A case marked `observe` records what it found and is never judged.
//!
//! This module is pure: the shape, its problems, and the judging of lines. [`run`] runs the steps.

pub mod run;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::intent::{Outcome, Problem, ProblemKind};
use crate::model::{Backend, Experiment};

/// Where a tool step's output and each observation are written, under the runtime folder, and
/// mounted read-only into every tool step at [`STEPS_MOUNT`].
pub const DIR: &str = "checks";
/// Where a tool step reads the output of the steps before it: `/vx/checks/<step>.log`.
pub const STEPS_MOUNT: &str = "/vx/checks";
/// The `op` of the line the runner adds after each step's own output.
pub const STEP_OP: &str = "vx.step";
/// The log stream every step's output is recorded under.
pub const STREAM: &str = "output";
/// `on: alone`: a namespace of the step's own, with loopback only.
pub const ALONE: &str = "alone";

const DEFAULT_TIMEOUT_S: f64 = 60.0;

/// An OCI image a step runs, and what every step of it is given.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    /// `<reference>@sha256:<digest>`, or a `localhost/` image from this host's Podman store.
    pub image: String,
    /// Put before each step's own `args`, such as the server a client talks to.
    #[serde(default)]
    pub args: Vec<String>,
    /// Replaces the image's entrypoint, to run another program the image holds.
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// What an `observe` step reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Observe {
    /// A container of a node's group: `running`, `restarts` and `started_at`.
    Member,
    /// A node's bridge port: `rx_packets`, `tx_packets`, `rx_bytes`, `tx_bytes`.
    Port,
}

impl Observe {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Port => "port",
        }
    }
}

/// One step. Exactly one of `tool`, `observe`, `wait`, `stop` and `sleep_s` says what it does.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Names the step's output; a tool or observe step has one.
    #[serde(default)]
    pub id: Option<String>,
    /// Run this tool.
    #[serde(default)]
    pub tool: Option<String>,
    /// A container node's id, whose network namespace the tool joins, or `alone`. For an
    /// `observe` step, the node it observes.
    #[serde(default)]
    pub on: Option<String>,
    /// A background step whose namespace the tool joins, such as a throwaway server's.
    #[serde(default)]
    pub with: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Package files mounted read-only, by the path in the container.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Start it and go on; a later `wait` or `stop` ends it, else the end of the checks.
    #[serde(default)]
    pub background: bool,
    /// A background step is ready when a line matches; the next step waits for that.
    #[serde(default)]
    pub ready: Option<BTreeMap<String, Json>>,
    /// The longest a foreground run, a readiness wait or a `wait` may take. Default 60 s.
    #[serde(default)]
    pub timeout_s: Option<f64>,
    /// Run the tool again until a line of the attempt matches, every `every_s`, `attempts` times
    /// at most.
    #[serde(default)]
    pub until: Option<BTreeMap<String, Json>>,
    #[serde(default)]
    pub every_s: Option<f64>,
    #[serde(default)]
    pub attempts: Option<u32>,
    #[serde(default)]
    pub observe: Option<Observe>,
    /// For `observe: member`: the member's name in the node's group (`kuksa-databroker`).
    #[serde(default)]
    pub member: Option<String>,
    /// Wait for this background step to exit.
    #[serde(default)]
    pub wait: Option<String>,
    /// Stop this background step.
    #[serde(default)]
    pub stop: Option<String>,
    #[serde(default)]
    pub sleep_s: Option<f64>,
}

impl Step {
    pub fn timeout_s(&self) -> f64 {
        self.timeout_s.unwrap_or(DEFAULT_TIMEOUT_S)
    }

    /// What it does, for messages.
    pub fn action(&self) -> &'static str {
        if self.tool.is_some() {
            "tool"
        } else if self.observe.is_some() {
            "observe"
        } else if self.wait.is_some() {
            "wait"
        } else if self.stop.is_some() {
            "stop"
        } else {
            "sleep"
        }
    }
}

/// How many lines a `count` expects.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Count {
    Exactly(u64),
    Rule(BTreeMap<CmpOp, u64>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Lt => "lt",
            Self::Le => "le",
            Self::Gt => "gt",
            Self::Ge => "ge",
        }
    }
}

/// One side of a `compare`: the field of the last line a step's `where` selects, or a value.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Side {
    #[serde(default)]
    pub step: Option<String>,
    #[serde(default, rename = "where")]
    pub select: BTreeMap<String, Json>,
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub value: Option<Json>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compare {
    pub lhs: Side,
    pub op: CmpOp,
    pub rhs: Side,
}

/// One case: the lines of `step` that `where` selects, and what is expected of them. With no
/// expectation, at least one line must match.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    /// What it checks, for people.
    #[serde(default)]
    pub statement: Option<String>,
    #[serde(default)]
    pub step: Option<String>,
    #[serde(default, rename = "where")]
    pub select: BTreeMap<String, Json>,
    /// A field of the selected lines, as a dotted path.
    #[serde(default)]
    pub field: Option<String>,
    /// The last selected line's field equals this.
    #[serde(default)]
    pub equals: Option<Json>,
    #[serde(default)]
    pub one_of: Option<Vec<Json>>,
    /// The last selected line's field holds this: a substring, a list element, or a map value.
    #[serde(default)]
    pub contains: Option<Json>,
    /// The last selected line's field is set (`true`) or absent (`false`).
    #[serde(default)]
    pub present: Option<bool>,
    /// How many lines are selected.
    #[serde(default)]
    pub count: Option<Count>,
    /// The selected lines' field values, in order, are exactly these.
    #[serde(default)]
    pub sequence: Option<Vec<Json>>,
    /// … end with these.
    #[serde(default)]
    pub ends_with: Option<Vec<Json>>,
    /// … hold these, in this order, with others between.
    #[serde(default)]
    pub includes: Option<Vec<Json>>,
    #[serde(default)]
    pub compare: Option<Compare>,
    /// Record what the case finds, never judge it: what the documentation under test does not say.
    #[serde(default)]
    pub observe: bool,
}

impl Case {
    fn expectations(&self) -> Vec<&'static str> {
        [
            ("equals", self.equals.is_some()),
            ("one_of", self.one_of.is_some()),
            ("contains", self.contains.is_some()),
            ("present", self.present.is_some()),
            ("count", self.count.is_some()),
            ("sequence", self.sequence.is_some()),
            ("ends_with", self.ends_with.is_some()),
            ("includes", self.includes.is_some()),
            ("compare", self.compare.is_some()),
        ]
        .into_iter()
        .filter_map(|(n, set)| set.then_some(n))
        .collect()
    }
}

/// The `checks` block.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checks {
    #[serde(default)]
    pub tools: BTreeMap<String, Tool>,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub cases: Vec<Case>,
}

impl Checks {
    /// The steps whose output is recorded, by id.
    pub fn recorded(&self) -> impl Iterator<Item = &str> {
        self.steps
            .iter()
            .filter(|s| s.tool.is_some() || s.observe.is_some())
            .filter_map(|s| s.id.as_deref())
    }

    pub fn case(&self, id: &str) -> Option<&Case> {
        self.cases.iter().find(|c| c.id == id)
    }
}

/// The entity a step's output is recorded under.
pub fn entity(step: &str) -> String {
    format!("checks/{step}")
}

/// The `checks` block's shape, or why it has none. Absent is `None`.
pub fn parse(block: Option<&serde_yaml_ng::Value>) -> Result<Option<Checks>, String> {
    block
        .map(|b| serde_yaml_ng::from_value(b.clone()).map_err(|e| e.to_string()))
        .transpose()
}

fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && id != "."
        && id != ".."
}

/// Whether `image` names content a run can reproduce: a digest, or this host's own store.
pub fn pinned(image: &str) -> bool {
    image.starts_with("localhost/")
        || image.split_once("@sha256:").is_some_and(|(r, d)| {
            !r.is_empty() && d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit())
        })
}

/// Every problem with the block's shape, read alone: tools, step order and references, cases.
/// The nodes it names are checked against the model by [`model_problems`].
pub fn problems(experiment: &str, checks: &Checks) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut push = |field: String, detail: String| {
        out.push(Problem {
            kind: ProblemKind::Check,
            experiment: experiment.to_string(),
            field,
            detail,
        })
    };
    for (id, t) in &checks.tools {
        if !plain_id(id) {
            push(
                format!("checks.tools.{id}"),
                "a tool id is letters, digits, `.`, `_` or `-`, at most 48".into(),
            );
        }
        if !pinned(&t.image) {
            push(
                format!("checks.tools.{id}.image"),
                format!(
                    "`{}` is neither pinned by digest (`<reference>@sha256:<64 hex>`) nor a \
                     `localhost/` image: a tag can move, so the run could not be repeated",
                    t.image
                ),
            );
        }
    }
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    // Background steps started so far, and whether each is still running.
    let mut background: BTreeMap<&str, bool> = BTreeMap::new();
    for (i, s) in checks.steps.iter().enumerate() {
        let at = |f: &str| format!("checks.steps[{i}]{f}");
        let actions = [
            s.tool.is_some(),
            s.observe.is_some(),
            s.wait.is_some(),
            s.stop.is_some(),
            s.sleep_s.is_some(),
        ];
        if actions.iter().filter(|a| **a).count() != 1 {
            push(
                at(""),
                "a step does exactly one of `tool`, `observe`, `wait`, `stop` and `sleep_s`".into(),
            );
            continue;
        }
        let recorded = s.tool.is_some() || s.observe.is_some();
        match (&s.id, recorded) {
            (Some(id), _) if !plain_id(id) => push(
                at(".id"),
                format!("`{id}` is not a step id: letters, digits, `.`, `_` or `-`, at most 48"),
            ),
            (Some(id), _) if !ids.insert(id) => {
                push(at(".id"), format!("step `{id}` is declared twice"))
            }
            (None, true) => push(
                at(".id"),
                format!("a `{}` step names its output with an `id`", s.action()),
            ),
            _ => {}
        }
        if s.timeout_s.is_some_and(|t| !(t > 0.0 && t.is_finite())) {
            push(
                at(".timeout_s"),
                "a timeout is a positive number of seconds".into(),
            );
        }
        if let Some(tool) = &s.tool {
            if !checks.tools.contains_key(tool) {
                push(
                    at(".tool"),
                    format!("no tool `{tool}` is declared under `checks.tools`"),
                );
            }
            match (&s.on, &s.with) {
                (Some(_), Some(_)) | (None, None) => push(
                    at(""),
                    "a tool step runs `on` a container node (or `alone`), or `with` a background \
                     step: one of them"
                        .into(),
                ),
                (None, Some(w)) if background.get(w.as_str()) != Some(&true) => push(
                    at(".with"),
                    format!("`{w}` is not a background step started earlier and still running"),
                ),
                _ => {}
            }
            for (inside, file) in &s.files {
                if !inside.starts_with('/') {
                    push(
                        at(&format!(".files.{inside}")),
                        "a file is mounted at an absolute path in the container".into(),
                    );
                }
                let below = std::path::Path::new(file)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)));
                if file.is_empty() || !below {
                    push(
                        at(&format!(".files.{inside}")),
                        format!("`{file}` is not a path inside the package"),
                    );
                }
                if inside == STEPS_MOUNT || inside.starts_with(&format!("{STEPS_MOUNT}/")) {
                    push(
                        at(&format!(".files.{inside}")),
                        format!("`{STEPS_MOUNT}` holds the steps' output"),
                    );
                }
            }
            if s.background {
                if s.until.is_some() || s.every_s.is_some() || s.attempts.is_some() {
                    push(at(".until"), "a background step is not repeated".into());
                }
            } else if s.ready.is_some() {
                push(
                    at(".ready"),
                    "only a background step waits to be ready".into(),
                );
            }
            if s.until.is_none() && (s.every_s.is_some() || s.attempts.is_some()) {
                push(
                    at(".until"),
                    "`every_s` and `attempts` repeat a step `until` a line matches".into(),
                );
            }
            if s.attempts == Some(0) {
                push(at(".attempts"), "a repeated step runs at least once".into());
            }
            if s.every_s.is_some_and(|t| !(t >= 0.0 && t.is_finite())) {
                push(at(".every_s"), "`every_s` is a number of seconds".into());
            }
        } else {
            let tool_only = [
                ("with", s.with.is_some()),
                ("args", !s.args.is_empty()),
                ("files", !s.files.is_empty()),
                ("background", s.background),
                ("ready", s.ready.is_some()),
                ("until", s.until.is_some()),
                ("every_s", s.every_s.is_some()),
                ("attempts", s.attempts.is_some()),
            ];
            for (field, set) in tool_only {
                if set {
                    push(
                        at(&format!(".{field}")),
                        format!("only a tool step takes `{field}`"),
                    );
                }
            }
        }
        if s.tool.is_none() && s.observe.is_none() && s.on.is_some() {
            push(
                at(".on"),
                "only a tool or observe step runs `on` a node".into(),
            );
        }
        match s.observe {
            Some(kind) => {
                match s.on.as_deref() {
                    None | Some(ALONE) => push(
                        at(".on"),
                        format!(
                            "`observe: {}` names the node it observes in `on`",
                            kind.as_str()
                        ),
                    ),
                    _ => {}
                }
                match (kind, &s.member) {
                    (Observe::Member, None) => {
                        push(at(".member"), "`observe: member` names the member".into())
                    }
                    (Observe::Port, Some(_)) => {
                        push(at(".member"), "`observe: port` takes no member".into())
                    }
                    _ => {}
                }
            }
            None if s.member.is_some() => push(
                at(".member"),
                "only `observe: member` takes a member".into(),
            ),
            None => {}
        }
        for (field, target) in [("wait", &s.wait), ("stop", &s.stop)] {
            if let Some(t) = target {
                if background.get(t.as_str()) != Some(&true) {
                    push(
                        at(&format!(".{field}")),
                        format!("`{t}` is not a background step started earlier and still running"),
                    );
                } else {
                    background.insert(t, false);
                }
            }
        }
        if s.sleep_s.is_some_and(|t| !(t >= 0.0 && t.is_finite())) {
            push(at(".sleep_s"), "`sleep_s` is a number of seconds".into());
        }
        if s.background
            && let Some(id) = &s.id
        {
            background.insert(id, true);
        }
    }
    let recorded: BTreeSet<&str> = checks.recorded().collect();
    let mut case_ids = BTreeSet::new();
    for (i, c) in checks.cases.iter().enumerate() {
        let at = |f: &str| format!("checks.cases[{i}]{f}");
        if !case_ids.insert(c.id.as_str()) {
            push(at(".id"), format!("case `{}` is declared twice", c.id));
        }
        let expect = c.expectations();
        if expect.len() > 1 {
            push(
                at(""),
                format!(
                    "a case expects one thing, and this one says {}",
                    expect.join(", ")
                ),
            );
        }
        let names_step = |s: &Option<String>| match s {
            Some(s) if recorded.contains(s.as_str()) => None,
            Some(s) => Some(format!("no tool or observe step `{s}`")),
            None => Some("a case names the `step` whose lines it reads".into()),
        };
        if let Some(k) = &c.compare {
            for (name, side) in [("lhs", &k.lhs), ("rhs", &k.rhs)] {
                let field = at(&format!(".compare.{name}"));
                match (&side.value, &side.step) {
                    (Some(_), None) if side.field.is_none() && side.select.is_empty() => {}
                    (Some(_), _) => push(field, "a side is a `value` or a step's `field`".into()),
                    (None, s) => {
                        if let Some(why) = names_step(s) {
                            push(field.clone(), why);
                        }
                        if side.field.is_none() {
                            push(
                                field,
                                "a side reads a `field` of the step's last selected line".into(),
                            );
                        }
                    }
                }
            }
            if c.step.is_some() || !c.select.is_empty() || c.field.is_some() {
                push(
                    at(""),
                    "a `compare` case names its steps in `lhs` and `rhs`".into(),
                );
            }
        } else {
            if let Some(why) = names_step(&c.step) {
                push(at(".step"), why);
            }
            let needs_field = expect.iter().any(|e| *e != "count");
            if needs_field && c.field.is_none() {
                push(at(".field"), format!("`{}` reads a `field`", expect[0]));
            }
        }
        if let Some(Count::Rule(r)) = &c.count
            && r.is_empty()
        {
            push(
                at(".count"),
                "a count names a number or a rule such as `{ ge: 1 }`".into(),
            );
        }
    }
    out
}

/// Every problem with the nodes the steps name, against the experiment's model: a tool runs on a
/// container node (its group's namespace), and an observed node is a device of the vehicles.
pub fn model_problems(model: &Experiment, checks: &Checks) -> Vec<Problem> {
    let mut out = Vec::new();
    for (i, s) in checks.steps.iter().enumerate() {
        let Some(on) = s.on.as_deref() else { continue };
        if on == ALONE && s.observe.is_none() {
            continue;
        }
        let device = model.entities.iter().find_map(|e| e.device(on));
        let detail = match device {
            None => Some(format!(
                "`{on}` is not a device of the experiment's vehicles"
            )),
            Some(d)
                if (s.tool.is_some() || s.observe == Some(Observe::Member))
                    && !matches!(d.backend, None | Some(Backend::Container)) =>
            {
                Some(format!(
                    "`{on}` is not a container node: a tool runs in a container node's group, \
                     and a guest has no namespace on the host to join"
                ))
            }
            _ => None,
        };
        if let Some(detail) = detail {
            out.push(Problem {
                kind: ProblemKind::Check,
                experiment: model.id.clone(),
                field: format!("checks.steps[{i}].on"),
                detail,
            });
        }
    }
    out
}

/// The package files the steps mount that `root` does not hold.
pub fn file_problems(experiment: &str, checks: &Checks, root: &std::path::Path) -> Vec<Problem> {
    let mut out = Vec::new();
    for (i, s) in checks.steps.iter().enumerate() {
        for (inside, file) in &s.files {
            if !root.join(file).is_file() {
                out.push(Problem {
                    kind: ProblemKind::Check,
                    experiment: experiment.to_string(),
                    field: format!("checks.steps[{i}].files.{inside}"),
                    detail: format!("the package holds no file `{file}`"),
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Judging lines.

/// A line of output as a case reads it: its JSON object, or `{ "text": <line> }` for a line that
/// is not one.
pub fn line(text: &str) -> Json {
    match serde_json::from_str::<Json>(text) {
        Ok(v @ Json::Object(_)) => v,
        _ => serde_json::json!({ "text": text }),
    }
}

/// Equal as the scripts compared: numbers by value (`10` is `10.0`), everything else exactly.
pub fn same(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Number(x), Json::Number(y)) => x.as_f64() == y.as_f64(),
        (Json::Array(x), Json::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Json::Object(x), Json::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

/// The value at a dotted path, an array element by its index.
pub fn field<'a>(v: &'a Json, path: &str) -> Option<&'a Json> {
    path.split('.')
        .try_fold(v, |v, seg| match v {
            Json::Object(m) => m.get(seg),
            Json::Array(a) => a.get(seg.parse::<usize>().ok()?),
            _ => None,
        })
        .filter(|v| !v.is_null())
}

/// Whether `line` has every field `select` names, each equal; a `null` wants the field unset.
pub fn matches(line: &Json, select: &BTreeMap<String, Json>) -> bool {
    select.iter().all(|(k, want)| match (field(line, k), want) {
        (None, Json::Null) => true,
        (Some(got), want) => same(got, want),
        (None, _) => false,
    })
}

/// What a case found in one trial: its outcome (none for an `observe` case), the value it read,
/// and why it did not pass.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaseResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Json>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

impl CaseResult {
    fn inconclusive(why: String) -> Self {
        Self {
            outcome: Some(Outcome::Inconclusive),
            value: None,
            why: Some(why),
        }
    }
}

fn show(v: &Json) -> String {
    match v {
        Json::String(s) => format!("`{s}`"),
        other => other.to_string(),
    }
}

fn cmp(op: CmpOp, a: &Json, b: &Json) -> Option<bool> {
    match op {
        CmpOp::Eq => Some(same(a, b)),
        CmpOp::Ne => Some(!same(a, b)),
        _ => {
            let (x, y) = (a.as_f64()?, b.as_f64()?);
            Some(match op {
                CmpOp::Lt => x < y,
                CmpOp::Le => x <= y,
                CmpOp::Gt => x > y,
                _ => x >= y,
            })
        }
    }
}

/// Judge `case` over one trial's step output, `lines` by step id. A step with no output at all
/// did not run in the trial, and a case that reads it is `inconclusive`: never a pass.
pub fn judge(case: &Case, lines: &BTreeMap<String, Vec<Json>>) -> CaseResult {
    let r = judge_inner(case, lines);
    if case.observe {
        CaseResult { outcome: None, ..r }
    } else {
        r
    }
}

fn step_lines<'a>(
    lines: &'a BTreeMap<String, Vec<Json>>,
    step: &str,
) -> Result<&'a [Json], CaseResult> {
    match lines.get(step) {
        Some(l) if !l.is_empty() => Ok(l),
        _ => Err(CaseResult::inconclusive(format!(
            "step `{step}` did not run in this trial"
        ))),
    }
}

fn side_value(side: &Side, lines: &BTreeMap<String, Vec<Json>>) -> Result<Json, CaseResult> {
    if let Some(v) = &side.value {
        return Ok(v.clone());
    }
    let step = side.step.as_deref().unwrap_or_default();
    let path = side.field.as_deref().unwrap_or_default();
    let selected = step_lines(lines, step)?
        .iter()
        .rfind(|l| matches(l, &side.select))
        .ok_or_else(|| {
            fail(
                None,
                format!("no line of step `{step}` matches its `where`"),
            )
        })?;
    field(selected, path)
        .cloned()
        .ok_or_else(|| fail(None, format!("step `{step}`'s line has no `{path}`")))
}

fn fail(value: Option<Json>, why: String) -> CaseResult {
    CaseResult {
        outcome: Some(Outcome::Fail),
        value,
        why: Some(why),
    }
}

fn judged(holds: bool, value: Json, why: impl FnOnce() -> String) -> CaseResult {
    CaseResult {
        outcome: Some(if holds { Outcome::Pass } else { Outcome::Fail }),
        why: (!holds).then(why),
        value: Some(value),
    }
}

fn judge_inner(case: &Case, lines: &BTreeMap<String, Vec<Json>>) -> CaseResult {
    if let Some(k) = &case.compare {
        let (l, r) = match (side_value(&k.lhs, lines), side_value(&k.rhs, lines)) {
            (Ok(l), Ok(r)) => (l, r),
            (Err(e), _) | (_, Err(e)) => return e,
        };
        let value = serde_json::json!([l, r]);
        return match cmp(k.op, &l, &r) {
            Some(holds) => judged(holds, value, || {
                format!("{} is not {} {}", show(&l), k.op.as_str(), show(&r))
            }),
            None => fail(
                Some(value),
                format!("{} and {} are not numbers", show(&l), show(&r)),
            ),
        };
    }
    let step = case.step.as_deref().unwrap_or_default();
    let all = match step_lines(lines, step) {
        Ok(l) => l,
        Err(e) => return e,
    };
    let selected: Vec<&Json> = all.iter().filter(|l| matches(l, &case.select)).collect();
    if let Some(count) = &case.count {
        let n = selected.len() as u64;
        let holds = match count {
            Count::Exactly(want) => n == *want,
            Count::Rule(rule) => rule
                .iter()
                .all(|(op, want)| cmp(*op, &Json::from(n), &Json::from(*want)).unwrap_or(false)),
        };
        return judged(holds, Json::from(n), || {
            format!("{n} lines of step `{step}` match")
        });
    }
    let Some(path) = case.field.as_deref() else {
        // No expectation: a matching line exists. An observe case records how many matched.
        let n = selected.len();
        return judged(n > 0 || case.observe, Json::from(n), || {
            format!("no line of step `{step}` matches")
        });
    };
    let values: Vec<Json> = selected
        .iter()
        .filter_map(|l| field(l, path).cloned())
        .collect();
    let in_order = |want: &[Json], got: &[Json], kind: &str| -> CaseResult {
        let holds = match kind {
            "sequence" => got.len() == want.len() && got.iter().zip(want).all(|(a, b)| same(a, b)),
            "ends_with" => {
                got.len() >= want.len()
                    && got[got.len() - want.len()..]
                        .iter()
                        .zip(want)
                        .all(|(a, b)| same(a, b))
            }
            _ => {
                let mut it = got.iter();
                want.iter().all(|w| it.any(|g| same(g, w)))
            }
        };
        judged(holds, Json::from(got.to_vec()), || {
            format!(
                "`{path}` of step `{step}` went {}",
                Json::from(got.to_vec())
            )
        })
    };
    if let Some(want) = &case.sequence {
        return in_order(want, &values, "sequence");
    }
    if let Some(want) = &case.ends_with {
        return in_order(want, &values, "ends_with");
    }
    if let Some(want) = &case.includes {
        return in_order(want, &values, "includes");
    }
    let last = selected.last().and_then(|l| field(l, path)).cloned();
    if let Some(want) = case.present {
        let set = last.is_some();
        return judged(set == want, last.unwrap_or(Json::Null), || {
            if want {
                format!("no selected line of step `{step}` has `{path}`")
            } else {
                format!("step `{step}`'s `{path}` is set")
            }
        });
    }
    let Some(got) = last else {
        if case.observe {
            return CaseResult {
                outcome: None,
                value: Some(Json::from(values)),
                why: None,
            };
        }
        return fail(
            None,
            if selected.is_empty() {
                format!("no line of step `{step}` matches")
            } else {
                format!("no selected line of step `{step}` has `{path}`")
            },
        );
    };
    if let Some(want) = &case.equals {
        let holds = same(&got, want);
        return judged(holds, got.clone(), || {
            format!("`{path}` is {}, not {}", show(&got), show(want))
        });
    }
    if let Some(want) = &case.one_of {
        let holds = want.iter().any(|w| same(&got, w));
        return judged(holds, got.clone(), || {
            format!(
                "`{path}` is {}, none of {}",
                show(&got),
                Json::from(want.clone())
            )
        });
    }
    if let Some(want) = &case.contains {
        let holds = match (&got, want) {
            (Json::String(s), Json::String(w)) => s.contains(w.as_str()),
            (Json::Array(a), w) => a.iter().any(|x| same(x, w)),
            (Json::Object(m), w) => m.values().any(|x| same(x, w)),
            _ => false,
        };
        return judged(holds, got.clone(), || {
            format!(
                "`{path}` is {}, which does not hold {}",
                show(&got),
                show(want)
            )
        });
    }
    // An observe case with a field records every selected value; a last value alone otherwise.
    if case.observe {
        return CaseResult {
            outcome: None,
            value: Some(if values.len() == 1 {
                got
            } else {
                Json::from(values)
            }),
            why: None,
        };
    }
    judged(true, got, String::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn checks(yaml: &str) -> Checks {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    fn lines(step: &str, l: &[Json]) -> BTreeMap<String, Vec<Json>> {
        BTreeMap::from([(step.to_string(), l.to_vec())])
    }

    fn case(yaml: &str) -> Case {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    const OK: &str = r#"
tools:
  kv2: { image: "localhost/vxperiments/kv2:1", args: [--server, "10.100.0.50:55555"] }
  broker: { image: "ghcr.io/eclipse-kuksa/kuksa-databroker@sha256:529d8c6d3cc223362667672978bcb8f92c4d356b25aedd336e7a2e004d7279bd" }
steps:
  - { id: info, tool: kv2, on: cabin1, args: [info] }
  - { id: sub, tool: kv2, on: cabin1, args: [subscribe, Vehicle.Speed], background: true,
      ready: { op: subscribe, result: open } }
  - { sleep_s: 0.5 }
  - { wait: sub, timeout_s: 30 }
  - { id: b, tool: broker, on: alone, args: [--insecure], background: true }
  - { id: l, tool: kv2, with: b, args: [list, Vehicle], files: { /vss.json: assets/vss.json } }
  - { stop: b }
  - { id: drop, tool: kv2, on: hpc1, args: [actuate, X, "false"],
      until: { op: actuate, code: UNAVAILABLE }, every_s: 0.5, attempts: 20 }
  - { id: st, observe: member, on: hpc1, member: kuksa-databroker }
  - { id: rx, observe: port, on: zone-rl }
cases:
  - { id: version, step: info, where: { op: info }, field: version, equals: "0.7.1" }
  - { id: order, step: sub, where: { op: update }, field: value, sequence: [51.5, 10] }
  - { id: same, compare: { lhs: { step: st, field: restarts }, op: eq, rhs: { value: 0 } } }
  - { id: n, step: l, where: { op: list }, count: { ge: 1 } }
  - { id: seen, step: drop, observe: true, field: code }
"#;

    #[test]
    fn a_well_formed_block_has_no_problems() {
        assert_eq!(problems("e", &checks(OK)), vec![]);
    }

    #[test]
    fn each_misuse_is_named_by_its_field() {
        let bad = checks(
            r#"
tools:
  t: { image: "ghcr.io/x/y:latest" }
steps:
  - { tool: t, on: a }
  - { id: s, tool: nope, on: a, with: z }
  - { id: s, tool: t, on: a, ready: { op: x } }
  - { wait: s }
  - { id: o, observe: member, on: a }
  - { id: p, tool: t, on: a, files: { rel: ../x } }
  - { sleep_s: 1, wait: s }
cases:
  - { id: c, step: missing, field: x, equals: 1 }
  - { id: c, step: s, equals: 1, count: 2 }
"#,
        );
        let fields: Vec<String> = problems("e", &bad).into_iter().map(|p| p.field).collect();
        for want in [
            "checks.tools.t.image",
            "checks.steps[0].id",
            "checks.steps[1].tool",
            "checks.steps[1]",
            "checks.steps[2].id",
            "checks.steps[2].ready",
            "checks.steps[3].wait",
            "checks.steps[4].member",
            "checks.steps[5].files.rel",
            "checks.steps[6]",
            "checks.cases[0].step",
            "checks.cases[1].id",
            "checks.cases[1]",
        ] {
            assert!(
                fields.iter().any(|f| f == want),
                "{want} missing from {fields:?}"
            );
        }
    }

    #[test]
    fn a_digest_or_a_local_image_is_pinned_and_a_tag_is_not() {
        assert!(pinned("localhost/vxperiments/kv2:1"));
        assert!(pinned(&format!("ghcr.io/a/b@sha256:{}", "a".repeat(64))));
        assert!(!pinned("ghcr.io/a/b:1.0"));
        assert!(!pinned("ghcr.io/a/b@sha256:abc"));
    }

    #[test]
    fn a_field_equals_on_the_last_selected_line_and_numbers_compare_by_value() {
        let l = lines(
            "s",
            &[
                json!({"op": "get", "value": 1}),
                json!({"op": "get", "value": 51.5}),
                json!({"op": "vx.step", "exit": 0}),
            ],
        );
        let c = case("{ id: c, step: s, where: { op: get }, field: value, equals: 51.5 }");
        assert_eq!(judge(&c, &l).outcome, Some(Outcome::Pass));
        let c = case("{ id: c, step: s, where: { op: get }, field: value, equals: 1 }");
        let r = judge(&c, &l);
        assert_eq!(r.outcome, Some(Outcome::Fail));
        assert_eq!(r.why.as_deref(), Some("`value` is 51.5, not 1"));
        let l = lines("s", &[json!({"op": "get", "value": 10.0})]);
        let c = case("{ id: c, step: s, field: value, equals: 10 }");
        assert_eq!(judge(&c, &l).outcome, Some(Outcome::Pass));
        let l = lines(
            "m",
            &[json!({"op": "metadata", "unit": "km/h", "min": null})],
        );
        let c = case(
            "{ id: c, step: m, where: { op: metadata, unit: km/h, min: null, max: null }, count: 1 }",
        );
        assert_eq!(judge(&c, &l).outcome, Some(Outcome::Pass));
    }

    #[test]
    fn a_step_that_did_not_run_is_inconclusive_never_a_pass() {
        let c = case("{ id: c, step: s, field: value, equals: 1 }");
        let r = judge(&c, &BTreeMap::new());
        assert_eq!(r.outcome, Some(Outcome::Inconclusive));
        assert_eq!(r.why.as_deref(), Some("step `s` did not run in this trial"));
    }

    #[test]
    fn sequences_end_and_include_in_order() {
        let l = lines(
            "sub",
            &[
                json!({"op": "subscribe", "result": "open"}),
                json!({"op": "update", "value": 51.5}),
                json!({"op": "update", "value": 10.0}),
                json!({"op": "update", "value": 20.0}),
            ],
        );
        let pass = |y: &str| judge(&case(y), &l).outcome == Some(Outcome::Pass);
        assert!(pass(
            "{ id: c, step: sub, where: { op: update }, field: value, sequence: [51.5, 10, 20] }"
        ));
        assert!(!pass(
            "{ id: c, step: sub, where: { op: update }, field: value, sequence: [10, 20] }"
        ));
        assert!(pass(
            "{ id: c, step: sub, where: { op: update }, field: value, ends_with: [10, 20] }"
        ));
        assert!(pass(
            "{ id: c, step: sub, where: { op: update }, field: value, includes: [51.5, 20] }"
        ));
        assert!(!pass(
            "{ id: c, step: sub, where: { op: update }, field: value, includes: [20, 51.5] }"
        ));
        assert!(pass(
            "{ id: c, step: sub, where: { op: update }, count: 3 }"
        ));
        assert!(pass(
            "{ id: c, step: sub, where: { op: update }, count: { ge: 1, lt: 4 } }"
        ));
        assert!(!pass("{ id: c, step: sub, where: { op: nope } }"));
    }

    #[test]
    fn contains_reads_strings_lists_and_map_values() {
        let l = lines(
            "p",
            &[json!({"op": "rejected", "errors": {"117": "ERROR_CODE_INVALID_ARGUMENT"}})],
        );
        let c = case(
            "{ id: c, step: p, where: { op: rejected }, field: errors, contains: ERROR_CODE_INVALID_ARGUMENT }",
        );
        assert_eq!(judge(&c, &l).outcome, Some(Outcome::Pass));
        let l = lines(
            "b",
            &[json!({"text": "TLS is not enabled. Default behavior"})],
        );
        let c = case("{ id: c, step: b, field: text, contains: TLS is not enabled }");
        assert_eq!(judge(&c, &l).outcome, Some(Outcome::Pass));
    }

    #[test]
    fn compare_reads_two_steps() {
        let mut l = lines("a", &[json!({"op": "port", "rx_packets": 10})]);
        l.insert("b".into(), vec![json!({"op": "port", "rx_packets": 13})]);
        let c = case(
            "{ id: c, compare: { lhs: { step: b, field: rx_packets }, op: gt, rhs: { step: a, field: rx_packets } } }",
        );
        let r = judge(&c, &l);
        assert_eq!(r.outcome, Some(Outcome::Pass));
        assert_eq!(r.value, Some(json!([13, 10])));
        let c = case(
            "{ id: c, compare: { lhs: { step: a, field: rx_packets }, op: gt, rhs: { step: b, field: rx_packets } } }",
        );
        assert_eq!(judge(&c, &l).why.as_deref(), Some("10 is not gt 13"));
    }

    #[test]
    fn an_observe_case_records_and_is_never_judged() {
        let l = lines(
            "a",
            &[json!({"op": "actuate", "result": "error", "code": "DEADLINE_EXCEEDED"})],
        );
        let r = judge(
            &case("{ id: c, step: a, where: { op: actuate }, field: code, observe: true }"),
            &l,
        );
        assert_eq!(r.outcome, None);
        assert_eq!(r.value, Some(json!("DEADLINE_EXCEEDED")));
        let r = judge(
            &case("{ id: c, step: a, where: { op: nope }, field: code, observe: true }"),
            &l,
        );
        assert_eq!((r.outcome, r.value), (None, Some(json!([]))));
    }

    /// A `cases` criterion names declared, judged cases; the checks need a duration to run in.
    #[test]
    fn criteria_name_declared_judged_cases() {
        let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(
            r#"
id: e
checks:
  tools: { t: { image: "localhost/t:1" } }
  steps: [ { id: s, tool: t, on: a } ]
  cases:
    - { id: ok, step: s }
    - { id: seen, step: s, field: x, observe: true }
criteria:
  - { id: c, cases: [ok, seen, nope] }
"#,
        )
        .unwrap();
        let problems = crate::intent::read(&doc).1;
        let details: Vec<String> = problems.iter().map(|p| p.detail.clone()).collect();
        assert!(
            details.iter().any(|d| d.contains("no case `nope`")),
            "{details:?}"
        );
        assert!(
            details
                .iter()
                .any(|d| d.contains("`seen` is an `observe` case")),
            "{details:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.kind == ProblemKind::Check && p.detail.contains("measure duration")),
            "{details:?}"
        );
    }

    #[test]
    fn a_line_that_is_not_json_reads_as_text() {
        assert_eq!(
            line("Listening on 127.0.0.1"),
            json!({"text": "Listening on 127.0.0.1"})
        );
        assert_eq!(line("{\"op\":\"x\"}"), json!({"op": "x"}));
        assert_eq!(line("[1]"), json!({"text": "[1]"}));
    }
}
