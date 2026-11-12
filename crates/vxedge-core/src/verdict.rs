//! **Verdicts** (`ARCHITECTURE.md` §6.0.3): an experiment's criteria judged over the archives of
//! its trials, from their records alone.
//!
//! Each trial gives each metric one value (decision X-24): the last sample of the metric's series
//! on its entity inside the trial's measure phase, or the whole archive when it records no phases.
//! A criterion's aggregates then run across the trials it selects. A `vxarchive/1` archive names
//! no entities, so its series is matched by name and labels, and refused as ambiguous when more
//! than one scrape target served it. It counts failed scrapes without placing them in time, so
//! they are noted on the value instead of making it incomplete.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::Path;

use serde::Serialize;
use serde_yaml_ng::Value;

use crate::intent::{self, Intent, Judgement, Metric, Outcome, SampleSource, Samples};

/// One sample of one series.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub ts_ns: i64,
    /// The entity it is about; `None` in a `vxarchive/1` archive.
    pub entity: Option<String>,
    /// The scrape target that served it, in a `vxarchive/1` archive.
    pub target: Option<String>,
    pub metric: String,
    pub labels: BTreeMap<String, String>,
    pub value: f64,
}

/// A span without data from an entity, or from one of its series.
#[derive(Debug, Clone, PartialEq)]
pub struct Gap {
    pub entity: String,
    pub metric: Option<String>,
    pub from_ns: i64,
    pub to_ns: i64,
}

/// What a verdict reads from one archive.
#[derive(Debug, Clone, Default)]
pub struct Records {
    /// `vxarchive/2`, or `vxarchive/1` for an archive that names none.
    pub archive: String,
    pub metadata: serde_json::Value,
    pub samples: Vec<Sample>,
    pub gaps: Vec<Gap>,
    /// The trial's phases, in order.
    pub phases: Vec<Phase>,
}

/// A trial entering a phase; a `measure` phase of a campaign with windows names its window.
#[derive(Debug, Clone, PartialEq)]
pub struct Phase {
    pub ts_ns: i64,
    pub phase: String,
    pub window: Option<String>,
}

const V1_SAMPLES: &str = "ingest/samples.jsonl";
const V2_SAMPLES: &str = "metrics/samples.jsonl.zst";
const V2_PHASES: &str = "phases.jsonl";

fn member(zip: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Result<Option<Vec<u8>>, String> {
    let mut file = match zip.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(format!("cannot read `{name}`: {e}")),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read `{name}`: {e}"))?;
    Ok(Some(bytes))
}

/// The labels of a `vxarchive/1` sample, written as `{key="value",…}`.
pub fn parse_v1_labels(text: &str) -> BTreeMap<String, String> {
    let body = text.trim().trim_start_matches('{').trim_end_matches('}');
    let mut out = BTreeMap::new();
    let mut chars = body.chars().peekable();
    loop {
        let key: String = chars
            .by_ref()
            .skip_while(|c| *c == ',' || c.is_whitespace())
            .take_while(|c| *c != '=')
            .collect();
        if key.is_empty() {
            break;
        }
        if chars.next() != Some('"') {
            break;
        }
        let mut value = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        value.push(match next {
                            'n' => '\n',
                            other => other,
                        });
                    }
                }
                '"' => break,
                other => value.push(other),
            }
        }
        out.insert(key.trim().to_string(), value);
    }
    out
}

fn lines(bytes: &[u8]) -> impl Iterator<Item = serde_json::Value> + '_ {
    bytes
        .split(|b| *b == b'\n')
        .filter_map(|l| serde_json::from_slice::<serde_json::Value>(l).ok())
}

/// Read the records of the archive at `path`, `vxarchive/1` or `vxarchive/2`.
pub fn read_archive(path: &Path) -> Result<Records, String> {
    let file =
        std::fs::File::open(path).map_err(|e| format!("cannot open `{}`: {e}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| format!("`{}` is not a results archive: {e}", path.display()))?;
    let metadata: serde_json::Value = member(&mut zip, "metadata.json")?
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| format!("`{}` has no readable `metadata.json`", path.display()))?;
    let archive = metadata["archive"]
        .as_str()
        .unwrap_or("vxarchive/1")
        .to_string();
    let mut records = Records {
        archive: archive.clone(),
        metadata,
        ..Records::default()
    };
    if archive == "vxarchive/1" {
        for v in member(&mut zip, V1_SAMPLES)?
            .iter()
            .flat_map(|b| lines(b).collect::<Vec<_>>())
        {
            let (Some(metric), Some(value), Some(ts)) = (
                v["metric"].as_str(),
                v["value"].as_f64(),
                v["ts_unix_ms"].as_i64(),
            ) else {
                continue;
            };
            records.samples.push(Sample {
                ts_ns: ts.saturating_mul(1_000_000),
                entity: None,
                target: v["target"].as_str().map(String::from),
                metric: metric.to_string(),
                labels: parse_v1_labels(v["labels"].as_str().unwrap_or("")),
                value,
            });
        }
        return Ok(records);
    }
    if let Some(compressed) = member(&mut zip, V2_SAMPLES)? {
        let bytes = zstd::decode_all(compressed.as_slice())
            .map_err(|e| format!("cannot decompress `{V2_SAMPLES}`: {e}"))?;
        for v in lines(&bytes) {
            match v["record"].as_str() {
                Some("sample") => {
                    let (Some(metric), Some(value), Some(ts)) = (
                        v["metric"].as_str(),
                        v["value"].as_f64(),
                        v["ts_ns"].as_i64(),
                    ) else {
                        continue;
                    };
                    let labels = v["labels"]
                        .as_object()
                        .map(|m| {
                            m.iter()
                                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                                .collect()
                        })
                        .unwrap_or_default();
                    records.samples.push(Sample {
                        ts_ns: ts,
                        entity: v["entity"].as_str().map(String::from),
                        target: None,
                        metric: metric.to_string(),
                        labels,
                        value,
                    });
                }
                Some("gap") => {
                    let (Some(entity), Some(from), Some(to)) = (
                        v["entity"].as_str(),
                        v["from_ns"].as_i64(),
                        v["to_ns"].as_i64(),
                    ) else {
                        continue;
                    };
                    records.gaps.push(Gap {
                        entity: entity.to_string(),
                        metric: v["metric"].as_str().map(String::from),
                        from_ns: from,
                        to_ns: to,
                    });
                }
                _ => {}
            }
        }
    }
    if let Some(bytes) = member(&mut zip, V2_PHASES)? {
        for v in lines(&bytes) {
            if let (Some(ts), Some(phase)) = (v["ts_ns"].as_i64(), v["phase"].as_str()) {
                records.phases.push(Phase {
                    ts_ns: ts,
                    phase: phase.to_string(),
                    window: v["window"].as_str().map(String::from),
                });
            }
        }
        records.phases.sort_by_key(|p| p.ts_ns);
    }
    Ok(records)
}

/// A metric's value in one trial, whether its telemetry was complete, and a note: why it has no
/// value, or what about the value cannot be known.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrialValue {
    pub value: Option<f64>,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

impl TrialValue {
    fn none(why: impl Into<String>) -> Self {
        TrialValue {
            value: None,
            complete: false,
            why: Some(why.into()),
        }
    }
}

/// The span the trial measured: from its `measure` phase (the one of `window`, when the metric
/// names one) to the phase after it, or the whole archive when it records no phases.
fn measure_window(records: &Records, window: Option<&str>) -> Result<(i64, i64), String> {
    if records.phases.is_empty() {
        return Ok((i64::MIN, i64::MAX));
    }
    let start = records
        .phases
        .iter()
        .position(|p| p.phase == "measure" && p.window.as_deref() == window)
        .ok_or_else(|| match window {
            Some(w) => format!("the trial records no measure window `{w}`"),
            None => "the trial records no measure phase".to_string(),
        })?;
    let from = records.phases[start].ts_ns;
    let to = records.phases[start + 1..]
        .first()
        .map_or(i64::MAX, |p| p.ts_ns);
    Ok((from, to))
}

fn series_value(records: &Records, m: &Metric, series: &str, window: (i64, i64)) -> TrialValue {
    let (from, to) = window;
    let v1 = records.archive == "vxarchive/1";
    let matching: Vec<&Sample> = records
        .samples
        .iter()
        .filter(|s| {
            s.metric == series
                && (v1 || s.entity.as_deref() == Some(m.entity.as_str()))
                && m.labels.iter().all(|(k, v)| s.labels.get(k) == Some(v))
                && s.ts_ns >= from
                && s.ts_ns < to
        })
        .collect();
    if v1 {
        let targets: BTreeSet<&str> = matching
            .iter()
            .filter_map(|s| s.target.as_deref())
            .collect();
        if targets.len() > 1 {
            return TrialValue::none(format!(
                "`{series}` is served by {} scrape targets, and a vxarchive/1 archive names no \
                 entities",
                targets.len()
            ));
        }
    }
    let Some(last) = matching.iter().max_by_key(|s| s.ts_ns) else {
        return TrialValue::none(format!(
            "no `{series}` sample on `{}` while the trial measured",
            m.entity
        ));
    };
    if v1 {
        // A vxarchive/1 archive counts its failed scrapes but cannot place them in time, and
        // its runs scraped on after the probe had stopped, so a count is not a gap in the series.
        let failed = records.metadata["ingest"]["scrapes_failed"]
            .as_u64()
            .unwrap_or(0);
        return TrialValue {
            value: Some(last.value),
            complete: true,
            why: (failed > 0).then(|| {
                format!(
                    "the run counted {failed} failed scrapes, which a vxarchive/1 archive cannot \
                     place in time"
                )
            }),
        };
    }
    let complete = !records.gaps.iter().any(|g| {
        g.entity == m.entity
            && g.metric.as_deref().is_none_or(|x| x == series)
            && g.from_ns < to
            && g.to_ns > from
    });
    TrialValue {
        value: Some(last.value),
        complete,
        why: (!complete).then(|| format!("`{}` has a gap while the trial measured", m.entity)),
    }
}

/// Every metric's value in the trial `records` hold.
pub fn values(records: &Records, metrics: &[Metric]) -> BTreeMap<String, TrialValue> {
    let mut out: BTreeMap<String, TrialValue> = BTreeMap::new();
    for m in metrics {
        let Some(series) = &m.series else {
            continue;
        };
        let v = match measure_window(records, m.window.as_deref()) {
            Err(why) => TrialValue::none(why),
            Ok(w) => series_value(records, m, series, w),
        };
        out.insert(m.id.clone(), v);
    }
    for m in metrics {
        let Some([a, b]) = m.ratio.as_deref() else {
            continue;
        };
        let (Some(a), Some(b)) = (out.get(a), out.get(b)) else {
            continue;
        };
        let v = match (a.value, b.value) {
            (Some(_), Some(0.0)) => TrialValue::none("the ratio divides by 0"),
            (Some(x), Some(y)) => TrialValue {
                value: Some(x / y * m.factor.unwrap_or(1.0)),
                complete: a.complete && b.complete,
                why: a.why.clone().or(b.why.clone()),
            },
            _ => TrialValue::none(
                a.why
                    .clone()
                    .or(b.why.clone())
                    .unwrap_or_else(|| "a member has no value".into()),
            ),
        };
        out.insert(m.id.clone(), v);
    }
    out
}

/// One trial: who it is, the archive it was read from, and its metrics' values.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trial {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arm: Option<String>,
    pub repetition: u32,
    pub parameters: BTreeMap<String, Value>,
    /// The archive's digest, or its path when the digest is not known.
    pub archive: String,
    pub values: BTreeMap<String, TrialValue>,
}

/// Who a trial is, as its archive's `metadata.trial` says, or as a caller says for an archive
/// written before campaigns.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Identity {
    pub id: String,
    pub arm: Option<String>,
    pub repetition: u32,
    pub parameters: BTreeMap<String, Value>,
}

impl Identity {
    /// The trial `metadata.trial` names, when a campaign wrote the archive.
    pub fn from_metadata(metadata: &serde_json::Value) -> Option<Identity> {
        let t = metadata.get("trial")?;
        let parameters = t["parameters"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), serde_yaml_ng::to_value(v).ok()?)))
                    .collect()
            })
            .unwrap_or_default();
        Some(Identity {
            id: t["trial_id"].as_str()?.to_string(),
            arm: t["arm"].as_str().map(String::from),
            repetition: t["repetition"].as_u64().unwrap_or(0) as u32,
            parameters,
        })
    }
}

/// Who a trial is, from its archive's file stem and a `pattern` such as
/// `rep{repetition}-load{load_mbps}` or `rep{repetition}-{arm}`, for an archive written before
/// campaigns named their trials. `{repetition}` and `{arm}` set those; any other name sets that
/// parameter, read as a YAML scalar. The id is `r<repetition>-<point>`, where the point is the arm
/// or the parameters as `name=value`, so repetitions of one point share it. `None` when the stem
/// does not match.
pub fn identity_from_name(pattern: &str, stem: &str) -> Option<Identity> {
    enum Part<'a> {
        Text(&'a str),
        Field(&'a str),
    }
    let mut parts = Vec::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        let close = rest[open..].find('}')? + open;
        if open > 0 {
            parts.push(Part::Text(&rest[..open]));
        }
        parts.push(Part::Field(&rest[open + 1..close]));
        rest = &rest[close + 1..];
    }
    if !rest.is_empty() {
        parts.push(Part::Text(rest));
    }
    // Match left to right; a field takes the shortest non-empty text before the next literal.
    fn go<'a>(parts: &[Part<'_>], s: &'a str, out: &mut Vec<&'a str>) -> bool {
        match parts.split_first() {
            None => s.is_empty(),
            Some((Part::Text(t), more)) => s.strip_prefix(t).is_some_and(|r| go(more, r, out)),
            Some((Part::Field(_), more)) => {
                for end in s.char_indices().map(|(i, _)| i).skip(1).chain([s.len()]) {
                    out.push(&s[..end]);
                    if go(more, &s[end..], out) {
                        return true;
                    }
                    out.pop();
                }
                false
            }
        }
    }
    let mut values = Vec::new();
    if !go(&parts, stem, &mut values) {
        return None;
    }
    let names = parts.iter().filter_map(|p| match p {
        Part::Field(n) => Some(*n),
        Part::Text(_) => None,
    });
    let mut who = Identity::default();
    for (name, value) in names.zip(values) {
        match name {
            "repetition" => who.repetition = value.parse().ok()?,
            "arm" => who.arm = Some(value.to_string()),
            _ => {
                let v = serde_yaml_ng::from_str(value).unwrap_or(Value::from(value));
                who.parameters.insert(name.to_string(), v);
            }
        }
    }
    let point = who.arm.clone().unwrap_or_else(|| {
        who.parameters
            .iter()
            .map(|(k, v)| {
                let text = serde_yaml_ng::to_string(v).unwrap_or_default();
                format!("{k}={}", text.trim())
            })
            .collect::<Vec<_>>()
            .join(",")
    });
    who.id = format!("r{}-{point}", who.repetition);
    Some(who)
}

/// The trial in the archive at `path`: `identity` when given, else its `metadata.trial`.
pub fn trial(
    path: &Path,
    archive: String,
    identity: Option<Identity>,
    metrics: &[Metric],
) -> Result<Trial, String> {
    let records = read_archive(path)?;
    let who = identity
        .or_else(|| Identity::from_metadata(&records.metadata))
        .ok_or_else(|| {
            format!(
                "`{}` names no trial: a campaign did not write it",
                path.display()
            )
        })?;
    Ok(Trial {
        id: who.id,
        arm: who.arm,
        repetition: who.repetition,
        parameters: who.parameters,
        archive,
        values: values(&records, metrics),
    })
}

fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// The trials of each reference campaign an experiment declares, by reference name, or why that
/// campaign could not be read.
pub type References = BTreeMap<String, Result<Vec<Trial>, String>>;

/// The archived trials of the campaign `id` that `root`'s `results/<id>/campaign.json` records,
/// read with `metrics`, as `ResultService` reads a reference outside the host. Each archive is
/// read from the record's folder (a relative path below it, or an absolute one by its file name)
/// and must hash to the record's `sha256`.
pub fn recorded_trials(
    root: &std::path::Path,
    id: &str,
    metrics: &[Metric],
) -> Result<Vec<Trial>, String> {
    use std::path::{Component, Path};
    let one_name = matches!(
        Path::new(id).components().collect::<Vec<_>>().as_slice(),
        [Component::Normal(_)]
    );
    if !one_name {
        return Err(format!("`{id}` is not one folder name under `results/`"));
    }
    let folder = root.join("results").join(id);
    let file = folder.join("campaign.json");
    let text = std::fs::read(&file)
        .map_err(|e| format!("no campaign record `{}`: {e}", file.display()))?;
    let rec: serde_json::Value = serde_json::from_slice(&text)
        .map_err(|e| format!("`{}` is not a campaign record: {e}", file.display()))?;
    if rec["campaign_id"].as_str() != Some(id) {
        return Err(format!(
            "`{}` does not record campaign `{id}`",
            file.display()
        ));
    }
    let mut out = Vec::new();
    for t in rec["trials"].as_array().into_iter().flatten() {
        let (Some(tid), Some(archive)) = (t["id"].as_str(), t["archive"].as_str()) else {
            continue;
        };
        if t["state"].as_str() != Some("succeeded") || archive.is_empty() {
            continue;
        }
        let named = Path::new(archive);
        let below = named
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        let path = if named.is_relative() && below {
            folder.join(named)
        } else if let Some(name) = named.is_absolute().then(|| named.file_name()).flatten() {
            folder.join(name)
        } else {
            return Err(format!(
                "trial `{tid}`: archive `{archive}` is outside the record's folder"
            ));
        };
        let want = t["sha256"].as_str().unwrap_or_default();
        let want = want.strip_prefix("sha256:").unwrap_or(want);
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("trial `{tid}`: no archive `{}`: {e}", path.display()))?;
        let got = crate::pkg::sha256(&bytes);
        if want.is_empty() || got.strip_prefix("sha256:") != Some(want) {
            return Err(format!(
                "trial `{tid}`: `{}` is {got}, the record says `{want}`",
                path.display()
            ));
        }
        let parameters = t["parameters"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), serde_yaml_ng::to_value(v).ok()?)))
                    .collect()
            })
            .unwrap_or_default();
        let who = Identity {
            id: tid.to_string(),
            arm: t["arm"]
                .as_str()
                .filter(|a| !a.is_empty())
                .map(String::from),
            repetition: t["repetition"].as_u64().unwrap_or(0) as u32,
            parameters,
        };
        out.push(trial(&path, got, Some(who), metrics)?);
    }
    Ok(out)
}

/// The references `intent` declares, each read from `root`'s `results/` with [`recorded_trials`].
pub fn recorded_references(root: &std::path::Path, intent: &Intent) -> References {
    intent
        .references
        .iter()
        .map(|(name, r)| {
            (
                name.clone(),
                recorded_trials(root, &r.campaign, &intent.metrics),
            )
        })
        .collect()
}

/// Trials, as [`intent::SampleSource`]: a metric's values under a selection are the values of
/// the trials it selects, in order; a reference's are those of its campaign's trials.
pub struct Trials<'a>(pub &'a [Trial], pub &'a References);

fn select_samples(
    trials: &[Trial],
    metric: &str,
    select: &BTreeMap<String, Value>,
) -> Option<Samples> {
    let chosen: Vec<&Trial> = trials
        .iter()
        .filter(|t| {
            select.iter().all(|(k, v)| match k.as_str() {
                "arm" => t.arm.as_deref().is_some_and(|a| Some(a) == v.as_str()),
                _ => t.parameters.get(k).is_some_and(|p| same(p, v)),
            })
        })
        .collect();
    if chosen.is_empty() {
        return None;
    }
    let mut s = Samples {
        complete: true,
        ..Samples::default()
    };
    for t in chosen {
        match t.values.get(metric) {
            Some(TrialValue {
                value: Some(v),
                complete,
                ..
            }) => {
                s.values.push(*v);
                s.trials.push(t.id.clone());
                s.complete &= complete;
            }
            _ => s.complete = false,
        }
    }
    Some(s)
}

impl SampleSource for Trials<'_> {
    fn samples(&self, metric: &str, select: &BTreeMap<String, Value>) -> Option<Samples> {
        select_samples(self.0, metric, select)
    }

    fn reference_samples(
        &self,
        reference: &str,
        metric: &str,
        select: &BTreeMap<String, Value>,
    ) -> Result<Option<Samples>, String> {
        match self.1.get(reference) {
            None => Err(format!("reference `{reference}` was not given")),
            Some(Err(why)) => Err(format!("reference `{reference}`: {why}")),
            Some(Ok(trials)) => Ok(select_samples(trials, metric, select)),
        }
    }
}

/// An experiment's verdict: every criterion judged, and the trials they were judged over.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    /// `pass` when every criterion passes, `fail` when one fails, else `inconclusive`; an
    /// experiment without criteria is `inconclusive`.
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hypothesis: Option<String>,
    pub criteria: Vec<Judgement>,
    pub trials: Vec<Trial>,
}

/// Judge `intent`'s criteria over `trials`, with no reference campaign.
pub fn verdict(intent: &Intent, trials: Vec<Trial>) -> Verdict {
    verdict_with(intent, trials, &References::new())
}

/// Judge `intent`'s criteria over `trials`, reading each declared reference from `references`.
/// A criterion that reads a reference missing from it is `inconclusive`.
pub fn verdict_with(intent: &Intent, trials: Vec<Trial>, references: &References) -> Verdict {
    let src = Trials(&trials, references);
    let criteria: Vec<Judgement> = intent
        .criteria
        .iter()
        .map(|c| intent::judge(c, &src))
        .collect();
    let has = |o| criteria.iter().any(|j| j.outcome == o);
    let outcome = if criteria.is_empty() || (has(Outcome::Inconclusive) && !has(Outcome::Fail)) {
        Outcome::Inconclusive
    } else if has(Outcome::Fail) {
        Outcome::Fail
    } else {
        Outcome::Pass
    };
    Verdict {
        outcome,
        hypothesis: intent.hypothesis.as_ref().map(|h| h.id.clone()),
        criteria,
        trials,
    }
}

/// The point a trial ran: its id without the `r<repetition>-` prefix, so the repetitions of one
/// point share it.
pub fn point(t: &Trial) -> &str {
    t.id.strip_prefix('r')
        .and_then(|rest| rest.split_once('-'))
        .filter(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .map_or(t.id.as_str(), |(_, p)| p)
}

/// One metric at one point, in a result and in its reference: each side's median and IQR over
/// its repetitions, and whether the result's median is within the reference's IQR of the
/// reference's median.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricDelta {
    pub metric: String,
    pub point: String,
    pub count: usize,
    pub reference_count: usize,
    pub median: f64,
    pub iqr: f64,
    pub reference_median: f64,
    pub reference_iqr: f64,
    /// `median - reference_median`.
    pub delta: f64,
    pub within_reference_iqr: bool,
}

/// One criterion's outcome in a result and in its reference.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CriterionDelta {
    pub id: String,
    pub outcome: Outcome,
    pub reference_outcome: Outcome,
}

/// A result against its reference, metric by metric at every point both ran, and criterion by
/// criterion.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    pub outcome: Outcome,
    pub reference_outcome: Outcome,
    pub metrics: Vec<MetricDelta>,
    pub criteria: Vec<CriterionDelta>,
    /// How many of `metrics` are within the reference's IQR.
    pub within: usize,
}

fn by_point<'a>(trials: &'a [Trial], metric: &str) -> Vec<(&'a str, Vec<f64>)> {
    let mut out: Vec<(&str, Vec<f64>)> = Vec::new();
    for t in trials {
        let Some(v) = t.values.get(metric).and_then(|v| v.value) else {
            continue;
        };
        let p = point(t);
        match out.iter_mut().find(|(q, _)| *q == p) {
            Some((_, vs)) => vs.push(v),
            None => out.push((p, vec![v])),
        }
    }
    out
}

/// Compare `trials` with `reference`, both judged by `intent`: every metric at every point both
/// ran, in the reference's order, and every criterion's outcome on each side.
pub fn compare(intent: &Intent, trials: Vec<Trial>, reference: Vec<Trial>) -> Comparison {
    compare_with(intent, trials, reference, &References::new())
}

/// [`compare`], with each side's criteria reading the declared references from `references`.
pub fn compare_with(
    intent: &Intent,
    trials: Vec<Trial>,
    reference: Vec<Trial>,
    references: &References,
) -> Comparison {
    let mut metrics = Vec::new();
    for m in &intent.metrics {
        let ours = by_point(&trials, &m.id);
        for (p, theirs) in by_point(&reference, &m.id) {
            let Some((_, mine)) = ours.iter().find(|(q, _)| *q == p) else {
                continue;
            };
            let stat = |v: &[f64], a: &str| intent::aggregate(v, a).unwrap_or(f64::NAN);
            let (median, iqr) = (stat(mine, "median"), stat(mine, "iqr"));
            let (reference_median, reference_iqr) = (stat(&theirs, "median"), stat(&theirs, "iqr"));
            let delta = median - reference_median;
            metrics.push(MetricDelta {
                metric: m.id.clone(),
                point: p.to_string(),
                count: mine.len(),
                reference_count: theirs.len(),
                median,
                iqr,
                reference_median,
                reference_iqr,
                delta,
                within_reference_iqr: delta.abs() <= reference_iqr,
            });
        }
    }
    let ours = verdict_with(intent, trials, references);
    let theirs = verdict_with(intent, reference, references);
    let criteria = ours
        .criteria
        .iter()
        .zip(&theirs.criteria)
        .map(|(a, b)| CriterionDelta {
            id: a.id.clone().unwrap_or_else(|| a.rule.clone()),
            outcome: a.outcome,
            reference_outcome: b.outcome,
        })
        .collect();
    Comparison {
        outcome: ours.outcome,
        reference_outcome: theirs.outcome,
        within: metrics.iter().filter(|m| m.within_reference_iqr).count(),
        metrics,
        criteria,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn zip_with(path: &Path, members: &[(&str, Vec<u8>)]) {
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in members {
            w.start_file(*name, opts).unwrap();
            w.write_all(bytes).unwrap();
        }
        w.finish().unwrap();
    }

    fn metric(yaml: &str) -> Metric {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn v1_labels_parse_as_the_exporter_writes_them() {
        assert_eq!(
            parse_v1_labels(r#"{quantile="0.99",path="a\"b"}"#),
            BTreeMap::from([
                ("quantile".into(), "0.99".into()),
                ("path".into(), "a\"b".into())
            ])
        );
        assert!(parse_v1_labels("").is_empty());
    }

    /// A v1 archive: the series' last sample of the whole run, matched by labels; a failed scrape
    /// is noted, as it cannot be placed in time; a second target makes it ambiguous.
    #[test]
    fn a_v1_trial_value_is_the_last_sample_of_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let line = |ts: i64, target: &str, q: &str, v: f64| {
            format!(
                "{{\"ts_unix_ms\":{ts},\"target\":\"{target}\",\"metric\":\"vxprobe_rtt_seconds\",\"labels\":\"{{quantile=\\\"{q}\\\"}}\",\"value\":{v}}}\n"
            )
        };
        let samples = [
            line(1, "a:9801", "0.99", 0.5),
            line(2, "a:9801", "0.99", 0.7),
            line(2, "a:9801", "0.5", 0.1),
        ]
        .concat();
        let path = dir.path().join("rep1-load0.zip");
        zip_with(
            &path,
            &[
                (
                    "metadata.json",
                    br#"{"ingest":{"scrapes_failed":0}}"#.to_vec(),
                ),
                (V1_SAMPLES, samples.clone().into_bytes()),
            ],
        );
        let p99 = metric(
            "{ id: p99, kind: gauge, unit: s, entity: measurements/ctl, series: vxprobe_rtt_seconds, labels: { quantile: \"0.99\" } }",
        );
        let r = read_archive(&path).unwrap();
        assert_eq!(r.archive, "vxarchive/1");
        let v = &values(&r, std::slice::from_ref(&p99))["p99"];
        assert_eq!((v.value, v.complete), (Some(0.7), true));

        zip_with(
            &path,
            &[
                (
                    "metadata.json",
                    br#"{"ingest":{"scrapes_failed":1}}"#.to_vec(),
                ),
                (V1_SAMPLES, samples.clone().into_bytes()),
            ],
        );
        let v = &values(&read_archive(&path).unwrap(), std::slice::from_ref(&p99))["p99"];
        assert_eq!((v.value, v.complete), (Some(0.7), true));
        assert!(
            v.why.as_deref().unwrap().contains("1 failed scrapes"),
            "{v:?}"
        );

        let two = samples + &line(3, "b:9801", "0.99", 0.9);
        zip_with(
            &path,
            &[
                ("metadata.json", b"{}".to_vec()),
                (V1_SAMPLES, two.into_bytes()),
            ],
        );
        let v = &values(&read_archive(&path).unwrap(), std::slice::from_ref(&p99))["p99"];
        assert_eq!(v.value, None);
        assert!(
            v.why.as_deref().unwrap().contains("2 scrape targets"),
            "{v:?}"
        );
    }

    /// A v2 archive: the last sample on the metric's entity inside the measure phase; a gap there
    /// makes it incomplete; a ratio metric divides two series in the same trial.
    #[test]
    fn a_v2_trial_value_is_the_last_sample_of_the_measure_phase() {
        let dir = tempfile::tempdir().unwrap();
        let s = |ts: i64, entity: &str, metric: &str, v: f64| {
            format!(
                "{{\"record\":\"sample\",\"ts_ns\":{ts},\"entity\":\"{entity}\",\"metric\":\"{metric}\",\"value\":{v}}}\n"
            )
        };
        let mut lines = [
            s(5, "measurements/ctl", "vxprobe_lost_total", 0.0),
            s(15, "measurements/ctl", "vxprobe_lost_total", 3.0),
            s(15, "measurements/ctl", "vxprobe_sent_total", 300.0),
            s(15, "measurements/other", "vxprobe_lost_total", 99.0),
            s(25, "measurements/ctl", "vxprobe_lost_total", 50.0),
        ]
        .concat();
        let phases = [(0, "warmup"), (10, "measure"), (20, "cooldown")]
            .iter()
            .map(|(ts, p)| format!("{{\"record\":\"phase\",\"ts_ns\":{ts},\"phase\":\"{p}\"}}\n"))
            .collect::<String>();
        let metadata = serde_json::json!({
            "archive": "vxarchive/2",
            "trial": { "trial_id": "r1-saturated", "arm": "saturated", "repetition": 1,
                       "parameters": { "load_mbps": 600 } },
        });
        let write = |path: &Path, lines: &str| {
            zip_with(
                path,
                &[
                    ("metadata.json", serde_json::to_vec(&metadata).unwrap()),
                    (V2_SAMPLES, zstd::encode_all(lines.as_bytes(), 3).unwrap()),
                    (V2_PHASES, phases.clone().into_bytes()),
                ],
            )
        };
        let path = dir.path().join("t.zip");
        write(&path, &lines);
        let metrics: Vec<Metric> = [
            "{ id: lost, kind: counter, unit: '1', entity: measurements/ctl, series: vxprobe_lost_total }",
            "{ id: sent, kind: counter, unit: '1', entity: measurements/ctl, series: vxprobe_sent_total }",
            "{ id: loss_pct, kind: gauge, unit: '%', entity: measurements/ctl, ratio: [lost, sent], factor: 100 }",
        ]
        .iter()
        .map(|y| metric(y))
        .collect();
        let t = trial(&path, "sha256:x".into(), None, &metrics).unwrap();
        assert_eq!(
            (t.id.as_str(), t.arm.as_deref()),
            ("r1-saturated", Some("saturated"))
        );
        assert_eq!(t.parameters["load_mbps"].as_u64(), Some(600));
        assert_eq!(t.values["lost"].value, Some(3.0));
        assert_eq!(t.values["loss_pct"].value, Some(1.0));
        assert!(t.values["loss_pct"].complete);

        lines.push_str(
            "{\"record\":\"gap\",\"entity\":\"measurements/ctl\",\"from_ns\":12,\"to_ns\":13,\"reason\":\"x\"}\n",
        );
        write(&path, &lines);
        let t = trial(&path, "sha256:x".into(), None, &metrics).unwrap();
        assert!(!t.values["lost"].complete);
        assert!(!t.values["loss_pct"].complete);
    }

    /// A campaign with windows: each windowed metric is the last sample inside its own window, and
    /// a ratio divides values from two windows of one trial.
    #[test]
    fn a_windowed_metric_reads_its_own_measure_window() {
        let dir = tempfile::tempdir().unwrap();
        let s = |ts: i64, metric: &str, v: f64| {
            format!(
                "{{\"record\":\"sample\",\"ts_ns\":{ts},\"entity\":\"measurements/probe\",\"metric\":\"{metric}\",\"value\":{v}}}\n"
            )
        };
        let lines = [
            s(11, "vxprobe_rtt_seconds_max", 0.009),
            s(12, "vxprobe_rtt_seconds_max", 0.010),
            s(25, "vxprobe_rtt_seconds_max", 0.002),
            s(26, "vxprobe_rtt_seconds_p50", 0.001),
        ]
        .concat()
        .replace("vxprobe_rtt_seconds_p50", "vxprobe_rtt_seconds_mean");
        let phase = |ts: i64, p: &str, w: Option<&str>| match w {
            Some(w) => format!(
                "{{\"record\":\"phase\",\"ts_ns\":{ts},\"phase\":\"{p}\",\"window\":\"{w}\"}}\n"
            ),
            None => format!("{{\"record\":\"phase\",\"ts_ns\":{ts},\"phase\":\"{p}\"}}\n"),
        };
        let phases = [
            phase(0, "warmup", None),
            phase(10, "measure", Some("cold")),
            phase(20, "measure", Some("steady")),
            phase(30, "cooldown", None),
        ]
        .concat();
        let metadata = serde_json::json!({
            "archive": "vxarchive/2",
            "trial": { "trial_id": "r1-defaults", "repetition": 1, "parameters": {} },
        });
        let path = dir.path().join("t.zip");
        zip_with(
            &path,
            &[
                ("metadata.json", serde_json::to_vec(&metadata).unwrap()),
                (V2_SAMPLES, zstd::encode_all(lines.as_bytes(), 3).unwrap()),
                (V2_PHASES, phases.into_bytes()),
            ],
        );
        let metrics: Vec<Metric> = [
            "{ id: cold_max, kind: gauge, unit: s, entity: measurements/probe, series: vxprobe_rtt_seconds_max, window: cold }",
            "{ id: steady_max, kind: gauge, unit: s, entity: measurements/probe, series: vxprobe_rtt_seconds_max, window: steady }",
            "{ id: steady_mean, kind: gauge, unit: s, entity: measurements/probe, series: vxprobe_rtt_seconds_mean, window: steady }",
            "{ id: ratio, kind: gauge, unit: '1', entity: measurements/probe, ratio: [cold_max, steady_mean] }",
            "{ id: warm_max, kind: gauge, unit: s, entity: measurements/probe, series: vxprobe_rtt_seconds_max, window: warm }",
        ]
        .iter()
        .map(|y| metric(y))
        .collect();
        let t = trial(&path, "sha256:x".into(), None, &metrics).unwrap();
        assert_eq!(t.values["cold_max"].value, Some(0.010));
        assert_eq!(t.values["steady_max"].value, Some(0.002));
        assert_eq!(t.values["steady_mean"].value, Some(0.001));
        assert_eq!(t.values["ratio"].value, Some(10.0));
        assert_eq!(t.values["warm_max"].value, None);
        assert!(
            t.values["warm_max"]
                .why
                .as_deref()
                .unwrap()
                .contains("no measure window `warm`"),
            "{:?}",
            t.values["warm_max"]
        );
    }

    fn t(id: &str, arm: &str, load: u64, p99: f64) -> Trial {
        Trial {
            id: id.into(),
            arm: Some(arm.into()).filter(|a: &String| !a.is_empty()),
            repetition: 1,
            parameters: BTreeMap::from([("load_mbps".into(), Value::from(load))]),
            archive: format!("{id}.zip"),
            values: BTreeMap::from([(
                "p99".into(),
                TrialValue {
                    value: Some(p99),
                    complete: true,
                    why: None,
                },
            )]),
        }
    }

    /// exp-001's H1 as the notebook states it, over five repetitions: the median of saturated
    /// p99 within 2x of the median of idle p99.
    #[test]
    fn a_verdict_judges_every_criterion_over_the_selected_trials() {
        let doc: Value = serde_yaml_ng::from_str(
            r#"
kind: experiment
id: exp-001
hypothesis: { id: H1, statement: s }
parameters:
  load_mbps: { type: integer, unit: Mbit/s, default: 0, min: 0, max: 1000 }
metrics:
  - { id: p99, kind: gauge, unit: s, entity: measurements/ctl, series: vxprobe_rtt_seconds,
      labels: { quantile: "0.99" } }
criteria:
  - id: h1
    lhs: { metric: p99, aggregate: median, select: { load_mbps: 600 } }
    op: le
    rhs: { metric: p99, aggregate: median, select: { arm: idle }, factor: 2.0 }
    min_samples: 5
campaign:
  arms: { idle: { load_mbps: 0 } }
  sweep: { load_mbps: [0, 600] }
"#,
        )
        .unwrap();
        let (intent, problems) = intent::read(&doc);
        assert_eq!(problems, vec![]);
        let mut trials = Vec::new();
        for r in 1..=5 {
            trials.push(t(&format!("r{r}-idle"), "idle", 0, 1.0 + r as f64 / 10.0));
            trials.push(t(
                &format!("r{r}-load_mbps=600"),
                "",
                600,
                2.0 + r as f64 / 10.0,
            ));
        }
        let v = verdict(&intent, trials.clone());
        assert_eq!(v.outcome, Outcome::Pass, "{v:?}");
        assert_eq!(v.criteria[0].lhs, Some(2.3));
        assert_eq!(v.criteria[0].rhs, Some(2.6));
        assert_eq!(v.criteria[0].reads[0].trials.len(), 5);

        // A trial that lost its value makes the criterion inconclusive, never a pass.
        trials[0].values.get_mut("p99").unwrap().value = None;
        let v = verdict(&intent, trials);
        assert_eq!(v.outcome, Outcome::Inconclusive);
        assert!(v.criteria[0].why.as_deref().unwrap().contains("incomplete"));
    }

    /// exp-010's H2: this campaign's p99 growth from 0 to 600 Mbit/s within 1.10x of the growth
    /// of a reference campaign (exp-001). A reference not given, or unreadable, is inconclusive.
    #[test]
    fn a_criterion_reads_a_declared_reference_campaign() {
        let doc: Value = serde_yaml_ng::from_str(
            r#"
kind: experiment
id: exp-010
hypothesis: { id: H2, statement: s }
references:
  vlan: { campaign: exp-001, statement: "the VLAN baseline, frozen" }
parameters:
  load_mbps: { type: integer, unit: Mbit/s, default: 0, min: 0, max: 1000 }
metrics:
  - { id: p99, kind: gauge, unit: s, entity: measurements/ctl, series: vxprobe_rtt_seconds,
      labels: { quantile: "0.99" } }
criteria:
  - id: h2
    op: le
    lhs: { ratio: [{ metric: p99, aggregate: median, select: { load_mbps: 600 } },
                   { metric: p99, aggregate: median, select: { load_mbps: 0 } }] }
    rhs: { ratio: [{ metric: p99, aggregate: median, select: { load_mbps: 600 }, reference: vlan },
                   { metric: p99, aggregate: median, select: { load_mbps: 0 }, reference: vlan }],
           factor: 1.10 }
campaign:
  sweep: { load_mbps: [0, 600] }
"#,
        )
        .unwrap();
        let (intent, problems) = intent::read(&doc);
        assert_eq!(problems, vec![]);
        let ours = vec![
            t("r1-defaults", "", 0, 1.0),
            t("r1-load_mbps=600", "", 600, 1.5),
        ];
        let theirs = vec![
            t("r1-idle", "idle", 0, 1.0),
            t("r1-saturated", "saturated", 600, 1.4),
        ];
        let refs = References::from([("vlan".to_string(), Ok(theirs.clone()))]);
        let v = verdict_with(&intent, ours.clone(), &refs);
        assert_eq!(v.outcome, Outcome::Pass, "{v:?}");
        assert_eq!(v.criteria[0].lhs, Some(1.5));
        assert!((v.criteria[0].rhs.unwrap() - 1.54).abs() < 1e-9);
        let from_ref: Vec<Option<&str>> = v.criteria[0]
            .reads
            .iter()
            .map(|r| r.reference.as_deref())
            .collect();
        assert_eq!(from_ref, [None, None, Some("vlan"), Some("vlan")]);
        assert_eq!(v.criteria[0].reads[2].trials, ["r1-saturated"]);

        // Growth beyond the tolerance fails.
        let worse = vec![
            t("r1-defaults", "", 0, 1.0),
            t("r1-load_mbps=600", "", 600, 1.6),
        ];
        assert_eq!(verdict_with(&intent, worse, &refs).outcome, Outcome::Fail);

        // Not given, or unreadable: inconclusive, never a pass, and the reason says which.
        let v = verdict(&intent, ours.clone());
        assert_eq!(v.outcome, Outcome::Inconclusive);
        assert!(
            v.criteria[0]
                .why
                .as_deref()
                .unwrap()
                .contains("reference `vlan` was not given")
        );
        let broken = References::from([("vlan".to_string(), Err("no record".to_string()))]);
        let v = verdict_with(&intent, ours, &broken);
        assert_eq!(v.outcome, Outcome::Inconclusive);
        assert!(
            v.criteria[0]
                .why
                .as_deref()
                .unwrap()
                .contains("reference `vlan`: no record"),
            "{:?}",
            v.criteria[0].why
        );
    }

    /// Repetitions of one point compare by median against the reference's median and IQR, as
    /// exp-001's reproducibility check does.
    #[test]
    fn a_comparison_is_per_metric_per_point_against_the_reference_iqr() {
        let doc: Value = serde_yaml_ng::from_str(
            "kind: experiment\nid: e\nmetrics:\n  - { id: p99, kind: gauge, unit: s, entity: m, series: s }\n",
        )
        .unwrap();
        let (intent, _) = intent::read(&doc);
        let set = |vals: &[(u32, &str, f64)]| -> Vec<Trial> {
            vals.iter()
                .map(|(r, p, v)| t(&format!("r{r}-{p}"), "", 0, *v))
                .collect()
        };
        let reference = set(&[
            (1, "idle", 10.0),
            (2, "idle", 11.0),
            (3, "idle", 12.0),
            (4, "idle", 13.0),
            (5, "idle", 14.0),
            (1, "load_mbps=600", 20.0),
        ]);
        let rerun = set(&[
            (1, "idle", 14.5),
            (2, "idle", 15.0),
            (1, "load_mbps=600", 30.0),
        ]);
        assert_eq!(point(&rerun[2]), "load_mbps=600");
        let c = compare(&intent, rerun, reference);
        assert_eq!(c.metrics.len(), 2);
        let idle = &c.metrics[0];
        assert_eq!(
            (idle.point.as_str(), idle.count, idle.reference_count),
            ("idle", 2, 5)
        );
        assert_eq!(
            (idle.reference_median, idle.reference_iqr, idle.median),
            (12.0, 3.0, 14.75)
        );
        assert!(idle.within_reference_iqr);
        assert!(!c.metrics[1].within_reference_iqr);
        assert_eq!(c.within, 1);
    }

    #[test]
    fn an_old_archive_is_named_by_a_pattern_over_its_file_name() {
        let w = identity_from_name("rep{repetition}-load{load_mbps}", "rep3-load600").unwrap();
        assert_eq!((w.id.as_str(), w.repetition), ("r3-load_mbps=600", 3));
        assert_eq!(w.parameters["load_mbps"].as_u64(), Some(600));
        let w = identity_from_name("rep{repetition}-{arm}", "rep2-microvm-mmio").unwrap();
        assert_eq!(
            (w.id.as_str(), w.arm.as_deref()),
            ("r2-microvm-mmio", Some("microvm-mmio"))
        );
        assert_eq!(
            point(&Trial {
                id: w.id,
                arm: w.arm,
                repetition: 2,
                parameters: BTreeMap::new(),
                archive: String::new(),
                values: BTreeMap::new()
            }),
            "microvm-mmio"
        );
        assert!(identity_from_name("rep{repetition}-load{load_mbps}", "rep1-container").is_none());
        assert!(identity_from_name("rep{repetition}", "repx").is_none());
    }
}
