//! `PackageService`: what a package is, from its documents, and the packages this host's cache
//! holds (`ARCHITECTURE.md` §6.0.7). `Inspect` reads a workspace folder, a packed `.vxp` file or a
//! cached package by digest, and touches nothing; `Pull` and `Import` put a package in the cache,
//! where `ManifestRef.package` addresses it for every other verb.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde_yaml_ng::Value;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};

use vxedge_api::v1::manifest_ref::Source;
use vxedge_api::v1::package_service_server::PackageService;
use vxedge_api::v1::{
    CachedPackage, CampaignArm, CampaignProfile, CampaignSweep, CriterionCases,
    CriterionComparison, CriterionGroup, CriterionOperand, Job, ManifestRef, PackageCampaign,
    PackageCriterion, PackageHypothesis, PackageImportChunk, PackageInfo, PackageInspectRequest,
    PackageList, PackageListRequest, PackageMeasurement, PackageMetric, PackageParameter,
    PackagePullRequest, PackageRequirements, ParamValue, PlanProblem, criterion_operand,
    package_criterion, param_value,
};
use vxedge_core::intent;

use crate::audit::Audit;
use crate::errors::{self, R};
use crate::jobs::JobEngine;

/// The daemon's part and package cache, set once at start, so `ManifestRef.package` resolves
/// wherever a manifest is resolved.
static CACHE_DIR: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn set_cache_dir(dir: &Path) {
    let _ = CACHE_DIR.set(dir.to_path_buf());
}

/// The unpacked folder of the cached package `digest`, or `PACKAGE_UNKNOWN`.
pub(crate) fn cached_dir(digest: &str) -> Result<PathBuf, Status> {
    let unknown = || {
        errors::precondition(
            R::PackageUnknown,
            format!(
                "package `{digest}` is not in this host's cache: pull it (`PackageService.Pull`, \
                 `vxedge pkg pull`) or import it first"
            ),
        )
    };
    let cache = CACHE_DIR.get().ok_or_else(unknown)?;
    vxedge_core::distribution::cached_package_dir(cache, digest)
        .map_err(|e| errors::invalid(R::InvalidRequest, e.to_string()))?
        .ok_or_else(unknown)
}

/// The most a client may upload in one `Import`.
const IMPORT_MAX_BYTES: usize = 4 << 30;

pub struct PackageSvc {
    pub engine: JobEngine,
    pub audit: Arc<Audit>,
    pub cache_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

/// Where `Inspect` reads the package from, and its digest when it is known without packing.
enum Located {
    Folder(PathBuf),
    Cached(PathBuf, String),
    Packed(tempfile::TempDir, String),
}

fn locate(mref: Option<&ManifestRef>) -> Result<Located, Status> {
    let bad = |e: String| errors::invalid(R::InvalidRequest, e);
    match mref.and_then(|m| m.source.as_ref()) {
        Some(Source::Package(digest)) => Ok(Located::Cached(cached_dir(digest)?, digest.clone())),
        Some(Source::Path(path)) => {
            let p = PathBuf::from(path);
            if p.is_dir() {
                return Ok(Located::Folder(p));
            }
            let bytes = std::fs::read(&p)
                .map_err(|e| bad(format!("cannot read `{path}` on the daemon host: {e}")))?;
            let digest = vxedge_core::pkg::sha256(&bytes);
            let dir = tempfile::tempdir().map_err(|e| Status::internal(e.to_string()))?;
            let root = dir.path().join("package");
            vxedge_core::pkg::unpack(&bytes, &root)
                .map_err(|e| errors::invalid(R::WorkspaceInvalid, format!("`{path}`: {e}")))?;
            // The TempDir lives as long as the source: `inspect` reads `package/` beneath it.
            Ok(Located::Packed(dir, digest))
        }
        Some(Source::InlineYaml(_)) => Err(bad(
            "a package is a folder, a packed `.vxp` or a cached digest, not an inline document"
                .into(),
        )),
        None => Err(bad("a package is required".into())),
    }
}

pub(crate) fn param_value(v: &Value, ty: Option<intent::ParamType>) -> ParamValue {
    use param_value::Value as V;
    let value = match (v, ty) {
        (Value::Bool(b), _) => V::Boolean(*b),
        (Value::Number(n), Some(intent::ParamType::Integer)) if n.as_i64().is_some() => {
            V::Integer(n.as_i64().unwrap_or_default())
        }
        (Value::Number(n), None) if n.is_i64() => V::Integer(n.as_i64().unwrap_or_default()),
        (Value::Number(n), _) => V::Number(n.as_f64().unwrap_or_default()),
        (Value::String(s), _) => V::Text(s.clone()),
        (other, _) => V::Text(
            serde_yaml_ng::to_string(other)
                .unwrap_or_default()
                .trim()
                .to_string(),
        ),
    };
    ParamValue { value: Some(value) }
}

fn type_name(t: intent::ParamType) -> &'static str {
    match t {
        intent::ParamType::Integer => "integer",
        intent::ParamType::Number => "number",
        intent::ParamType::Boolean => "boolean",
        intent::ParamType::String => "string",
    }
}

/// The scenario the experiment runs on, typed, with what its storyboard declares. `None` when
/// it names none, or names one the package does not hold (a problem reported on its own).
fn scenario_info(ws: &vxedge_core::Workspace) -> Option<vxedge_api::v1::PackageScenario> {
    use vxedge_api::v1::{PackageScenario, ScenarioParameter};
    let id = ws
        .experiments
        .first()
        .and_then(vxedge_core::workspace::ExperimentDoc::scenario_ref)?;
    let s = ws.scenarios.iter().find(|s| s.id == id)?;
    let board = std::fs::read_to_string(ws.root.join(&s.storyboard))
        .ok()
        .and_then(|t| vxedge_core::scenario::read_storyboard(&t).ok())
        .unwrap_or_default();
    Some(PackageScenario {
        id: s.id.clone(),
        road: s.road.clone(),
        road_network: ws
            .road_docs
            .iter()
            .find(|r| r.id == s.road)
            .map(|r| r.network.clone())
            .unwrap_or_default(),
        storyboard: s.storyboard.clone(),
        simulator: s.simulator.clone(),
        clock_tick_ms: s.clock.as_ref().map_or(0, |c| c.tick_ms),
        clock_mode: s.clock.as_ref().map(|c| c.mode.clone()).unwrap_or_default(),
        seed: s.seed,
        actors: s
            .actors
            .iter()
            .map(|(entity, a)| (entity.clone(), a.vehicle.clone()))
            .collect(),
        entities: board.entities.iter().cloned().collect(),
        parameters: s
            .parameters
            .iter()
            .map(|(name, p)| {
                let declared = board.parameters.get(name);
                ScenarioParameter {
                    name: name.clone(),
                    unit: p.unit.clone().unwrap_or_default(),
                    min: p.min,
                    max: p.max,
                    declared_type: declared
                        .map(|d| d.parameter_type.clone())
                        .unwrap_or_default(),
                    declared_value: declared.map(|d| d.value.clone()).unwrap_or_default(),
                }
            })
            .collect(),
    })
}

fn kind_name(k: intent::MetricKind) -> &'static str {
    match k {
        intent::MetricKind::Counter => "counter",
        intent::MetricKind::Gauge => "gauge",
        intent::MetricKind::Histogram => "histogram",
    }
}

fn op_name(op: intent::Op) -> &'static str {
    match op {
        intent::Op::Lt => "lt",
        intent::Op::Le => "le",
        intent::Op::Gt => "gt",
        intent::Op::Ge => "ge",
        intent::Op::Within => "within",
        intent::Op::Outside => "outside",
    }
}

fn op_symbol(op: intent::Op) -> &'static str {
    match op {
        intent::Op::Lt => "<",
        intent::Op::Le => "<=",
        intent::Op::Gt => ">",
        intent::Op::Ge => ">=",
        intent::Op::Within => "within",
        intent::Op::Outside => "outside",
    }
}

fn value_text(v: &Value) -> String {
    serde_yaml_ng::to_string(v)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn operand_text(o: &intent::Operand) -> String {
    match o {
        intent::Operand::Constant(c) => format!("{} {}", c.value, c.unit),
        intent::Operand::Metric(m) => {
            let select = if m.select.is_empty() {
                String::new()
            } else {
                let parts: Vec<String> = m
                    .select
                    .iter()
                    .map(|(k, v)| format!("{k}={}", value_text(v)))
                    .collect();
                format!("[{}]", parts.join(", "))
            };
            let factor = m.factor.map(|f| format!("{f} x ")).unwrap_or_default();
            let offset = m.offset.map(|o| format!(" + {o}")).unwrap_or_default();
            format!("{factor}{}({}){select}{offset}", m.aggregate, m.metric)
        }
        intent::Operand::Combined(k) => {
            let Some((rule, members)) = k.rule() else {
                return "?".into();
            };
            let inner: Vec<String> = members.iter().map(operand_text).collect();
            let body = match rule {
                "ratio" => format!("({})", inner.join(" / ")),
                "diff" => format!("({})", inner.join(" - ")),
                _ => format!("{rule}({})", inner.join(", ")),
            };
            let factor = k.factor.map(|f| format!("{f} x ")).unwrap_or_default();
            let offset = k.offset.map(|o| format!(" + {o}")).unwrap_or_default();
            format!("{factor}{body}{offset}")
        }
    }
}

fn tolerance_text(t: &intent::Tolerance) -> String {
    match t {
        intent::Tolerance::Value(v) => v.to_string(),
        intent::Tolerance::Operand(o) => operand_text(o),
    }
}

fn operand(o: &intent::Operand) -> CriterionOperand {
    use criterion_operand::{Constant, MetricOperand, Operand};
    CriterionOperand {
        operand: Some(match o {
            intent::Operand::Constant(c) => Operand::Constant(Constant {
                value: c.value,
                unit: c.unit.clone(),
            }),
            intent::Operand::Metric(m) => Operand::Metric(MetricOperand {
                metric: m.metric.clone(),
                aggregate: m.aggregate.clone(),
                select: m
                    .select
                    .iter()
                    .map(|(k, v)| (k.clone(), param_value(v, None)))
                    .collect(),
                factor: m.factor,
                offset: m.offset,
            }),
            intent::Operand::Combined(k) => {
                let (rule, members) = k.rule().unwrap_or(("", &[]));
                Operand::Combined(criterion_operand::Combined {
                    rule: rule.to_string(),
                    operands: members.iter().map(operand).collect(),
                    factor: k.factor,
                    offset: k.offset,
                })
            }
        }),
    }
}

fn criterion(c: &intent::Criterion) -> PackageCriterion {
    match c {
        intent::Criterion::Comparison(cmp) => PackageCriterion {
            id: cmp.id.clone(),
            text: format!(
                "{} {} {}{}",
                operand_text(&cmp.lhs),
                op_symbol(cmp.op),
                operand_text(&cmp.rhs),
                cmp.tolerance
                    .as_ref()
                    .map(|t| format!(" ± {}", tolerance_text(t)))
                    .unwrap_or_default()
            ),
            guards: cmp.guards.iter().map(criterion).collect(),
            criterion: Some(package_criterion::Criterion::Comparison(
                CriterionComparison {
                    op: op_name(cmp.op).to_string(),
                    lhs: Some(operand(&cmp.lhs)),
                    rhs: Some(operand(&cmp.rhs)),
                    tolerance: match &cmp.tolerance {
                        Some(intent::Tolerance::Value(v)) => Some(*v),
                        _ => None,
                    },
                    min_samples: cmp.min_samples,
                    tolerance_operand: match &cmp.tolerance {
                        Some(intent::Tolerance::Operand(o)) => Some(operand(o)),
                        _ => None,
                    },
                },
            )),
        },
        intent::Criterion::Cases(k) => PackageCriterion {
            id: k.id.clone(),
            text: format!("cases({})", k.cases.join(", ")),
            criterion: Some(package_criterion::Criterion::Cases(CriterionCases {
                cases: k.cases.clone(),
            })),
            guards: Vec::new(),
        },
        intent::Criterion::Group(g) => {
            let (rule, members) = match (&g.all, &g.any) {
                (Some(all), _) => ("all", all.as_slice()),
                (None, Some(any)) => ("any", any.as_slice()),
                (None, None) => ("all", &[][..]),
            };
            let members: Vec<PackageCriterion> = members.iter().map(criterion).collect();
            let text = format!(
                "{rule}({})",
                members
                    .iter()
                    .map(|m| m.id.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            PackageCriterion {
                id: g.id.clone().unwrap_or_default(),
                text,
                criterion: Some(package_criterion::Criterion::Group(CriterionGroup {
                    rule: rule.to_string(),
                    members,
                })),
                guards: g.guards.iter().map(criterion).collect(),
            }
        }
    }
}

fn campaign(c: &intent::Campaign, params: &BTreeMap<String, intent::Parameter>) -> PackageCampaign {
    let ty = |name: &str| params.get(name).map(|p| p.ty);
    PackageCampaign {
        arms: c
            .arms
            .iter()
            .map(|(name, values)| CampaignArm {
                name: name.clone(),
                parameters: values
                    .iter()
                    .map(|(k, v)| (k.clone(), param_value(v, ty(k))))
                    .collect(),
            })
            .collect(),
        sweep: c
            .sweep
            .iter()
            .map(|(name, values)| CampaignSweep {
                parameter: name.clone(),
                values: values.iter().map(|v| param_value(v, ty(name))).collect(),
            })
            .collect(),
        repetitions: c.repetitions,
        warmup_s: c.warmup_s,
        duration_s: c.duration_s,
        profiles: c
            .profiles
            .iter()
            .map(|(name, p)| CampaignProfile {
                name: name.clone(),
                repetitions: p.repetitions,
                warmup_s: p.warmup_s,
                duration_s: p.duration_s,
                windows: p.windows.clone().into_iter().collect(),
            })
            .collect(),
    }
}

fn intent_reason(kind: intent::ProblemKind) -> R {
    match kind {
        intent::ProblemKind::Parameter => R::ParameterInvalid,
        intent::ProblemKind::Criterion => R::CriterionInvalid,
        intent::ProblemKind::Measurement => R::MeasurementInvalid,
        intent::ProblemKind::Scenario => R::ScenarioInvalid,
        intent::ProblemKind::Check => R::CheckInvalid,
    }
}

/// The `PackageInfo` of the package at `root`, read from its documents.
pub(crate) fn inspect(
    root: &Path,
    digest: Option<String>,
    runtime_dir: &Path,
) -> Result<PackageInfo, Status> {
    let ws = vxedge_core::Workspace::load(root)
        .map_err(|e| errors::invalid(R::WorkspaceInvalid, format!("`{}`: {e}", root.display())))?;
    let mut info = PackageInfo::default();
    let mut problems: Vec<PlanProblem> = Vec::new();
    let subject = ws
        .experiments
        .first()
        .map(|e| e.id.clone())
        .unwrap_or_default();
    if let Some(p) = &ws.package {
        info.id = p.id().to_string();
        info.version = p.version().unwrap_or_default();
        info.schema = p.schema().unwrap_or("legacy").to_string();
        info.entry = p.entry().unwrap_or_default().to_string();
        let r = p.host_requires();
        info.requires = Some(PackageRequirements {
            gpu: r.gpu.unwrap_or_else(|| "none".into()),
            kvm: r.kvm,
            preempt_rt: r
                .preempt_rt
                .map(|p| format!("{p:?}").to_lowercase())
                .unwrap_or_default(),
            backends: r.backends.iter().map(|b| b.as_str().to_string()).collect(),
        });
    }
    info.experiment_id = subject.clone();
    info.digest = match digest {
        Some(d) => d,
        None => vxedge_core::pkg::package_digest(root).unwrap_or_default(),
    };
    match vxedge_core::pkg::lock_problems(root) {
        Ok(lp) => {
            info.locked =
                vxedge_core::pkg::read_lock(root).ok().flatten().is_some() && lp.is_empty();
            info.lock_problems = lp;
        }
        Err(e) => info.lock_problems = vec![e.to_string()],
    }
    for e in ws.validate() {
        problems.push(PlanProblem {
            reason: R::WorkspaceInvalid as i32,
            subject: subject.clone(),
            detail: e.to_string(),
        });
    }
    if let Some(doc) = ws.experiments_raw.first() {
        let (typed, intent_problems) = intent::read(doc);
        info.hypothesis = typed.hypothesis.as_ref().map(|h| PackageHypothesis {
            id: h.id.clone(),
            statement: h.statement.clone(),
        });
        info.parameters = typed
            .parameters
            .iter()
            .map(|(name, p)| PackageParameter {
                name: name.clone(),
                r#type: type_name(p.ty).to_string(),
                unit: p.unit.clone().unwrap_or_default(),
                default: Some(param_value(&p.default, Some(p.ty))),
                min: p.min,
                max: p.max,
                values: p
                    .values
                    .iter()
                    .flatten()
                    .map(|v| param_value(v, Some(p.ty)))
                    .collect(),
                binds: p.binds.clone().unwrap_or_default(),
            })
            .collect();
        info.metrics = typed
            .metrics
            .iter()
            .map(|m| PackageMetric {
                id: m.id.clone(),
                kind: kind_name(m.kind).to_string(),
                unit: m.unit.clone(),
                entity: m.entity.clone(),
                series: m.series.clone().unwrap_or_default(),
                labels: m.labels.clone().into_iter().collect(),
                ratio: m.ratio.clone().unwrap_or_default(),
                factor: m.factor,
                window: m.window.clone().unwrap_or_default(),
                reduce: if m.series.is_some() {
                    m.reduction().to_string()
                } else {
                    String::new()
                },
            })
            .collect();
        info.criteria = typed.criteria.iter().map(criterion).collect();
        info.campaign = typed
            .campaign
            .as_ref()
            .map(|c| campaign(c, &typed.parameters));
        problems.extend(intent_problems.into_iter().map(|p| PlanProblem {
            reason: intent_reason(p.kind) as i32,
            subject: subject.clone(),
            detail: p.to_string(),
        }));
    }
    problems.extend(ws.scenario_problems().into_iter().map(|p| PlanProblem {
        reason: intent_reason(p.kind) as i32,
        subject: subject.clone(),
        detail: p.to_string(),
    }));
    info.scenario = scenario_info(&ws);
    let text = vxedge_core::Workspace::read_text(root).unwrap_or_default();
    match vxedge_core::model::load_yaml(&text) {
        Ok(model) => {
            info.measurements = model
                .measurements
                .iter()
                .map(|m| PackageMeasurement {
                    id: m.id.clone(),
                    kind: m.kind.as_str().to_string(),
                    from: m.from.clone(),
                    to: m.to.clone(),
                    slice: m.slice.clone(),
                    rate_pps: m.rate_pps,
                    payload_bytes: m.payload_bytes,
                    entity: m.entity(),
                })
                .collect();
            problems.extend(
                vxedge_core::measurements::problems(&model)
                    .into_iter()
                    .map(|p| PlanProblem {
                        reason: intent_reason(p.kind) as i32,
                        subject: subject.clone(),
                        detail: p.to_string(),
                    }),
            );
            info.entities = crate::telemetry::entities(&model, runtime_dir)
                .into_iter()
                .map(crate::plan::planned_entity)
                .collect();
        }
        Err(e) => problems.push(PlanProblem {
            reason: R::WorkspaceInvalid as i32,
            subject: subject.clone(),
            detail: e.to_string(),
        }),
    }
    info.problems = problems;
    Ok(info)
}

#[tonic::async_trait]
impl PackageService for PackageSvc {
    async fn inspect(
        &self,
        req: Request<PackageInspectRequest>,
    ) -> Result<Response<PackageInfo>, Status> {
        let source = locate(req.get_ref().package.as_ref())?;
        let runtime_dir = self.runtime_dir.clone();
        let info = tokio::task::spawn_blocking(move || {
            let (root, digest) = match &source {
                Located::Folder(p) => (p.clone(), None),
                Located::Cached(p, d) => (p.clone(), Some(d.clone())),
                Located::Packed(t, d) => (t.path().join("package"), Some(d.clone())),
            };
            let out = inspect(&root, digest, &runtime_dir);
            drop(source);
            out
        })
        .await
        .map_err(|e| Status::internal(format!("inspect task panicked: {e}")))??;
        Ok(Response::new(info))
    }

    async fn list(&self, _: Request<PackageListRequest>) -> Result<Response<PackageList>, Status> {
        let cache = self.cache_dir.clone();
        let packages =
            tokio::task::spawn_blocking(move || vxedge_core::distribution::cached(&cache))
                .await
                .map_err(|e| Status::internal(format!("list task panicked: {e}")))?
                .into_iter()
                .map(|p| CachedPackage {
                    digest: p.digest,
                    id: p.id,
                    version: p.version,
                    size_bytes: p.size_bytes,
                    has_lock: p.has_lock,
                })
                .collect();
        Ok(Response::new(PackageList { packages }))
    }

    async fn pull(&self, req: Request<PackagePullRequest>) -> Result<Response<Job>, Status> {
        let opts = crate::jobs::JobOpts::request(&req, &req.get_ref().request_id);
        let reference = req.into_inner().reference;
        let (_, digest) = vxedge_core::distribution::split_pull_ref(&reference)
            .map_err(|e| errors::invalid(R::InvalidRequest, e.to_string()))?;
        // As `up` does, and before anything is fetched: under a policy that requires signatures,
        // a package with no verification record from an allowed signer is refused.
        crate::plan::trust_check(Some(digest))
            .map_err(|detail| errors::precondition(R::SignatureInvalid, detail))?;
        self.audit.record(
            "PackageService.Pull",
            serde_json::json!({ "reference": reference }),
        );
        let cache = self.cache_dir.clone();
        let job = self
            .engine
            .submit_with(opts, "package.pull", move |p| async move {
                p.update(10, &format!("pulling {reference}"));
                let (r, c) = (reference.clone(), cache.clone());
                let file =
                    tokio::task::spawn_blocking(move || vxedge_core::distribution::pull(&r, &c))
                        .await
                        .map_err(|e| format!("pull task panicked: {e}"))?
                        .map_err(|e| e.to_string())?;
                let digest = reference
                    .rsplit_once('@')
                    .map(|(_, d)| d.to_string())
                    .unwrap_or_default();
                p.artifact(&file, digest.trim_start_matches("sha256:"));
                Ok::<_, crate::jobs::JobFailure>(format!("package {digest} in the cache"))
            })
            .await?;
        Ok(Response::new(job))
    }

    async fn import(
        &self,
        req: Request<Streaming<PackageImportChunk>>,
    ) -> Result<Response<Job>, Status> {
        let mut stream = req.into_inner();
        let mut digest = String::new();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if digest.is_empty() {
                digest = chunk.digest;
            }
            bytes.extend_from_slice(&chunk.data);
            if bytes.len() > IMPORT_MAX_BYTES {
                return Err(errors::invalid(
                    R::InvalidRequest,
                    format!("a package import is at most {IMPORT_MAX_BYTES} bytes"),
                ));
            }
        }
        vxedge_core::distribution::package_cache_path(&self.cache_dir, &digest).map_err(|_| {
            errors::invalid(
                R::InvalidRequest,
                "the first chunk names the package digest, `sha256:<hex>`",
            )
        })?;
        self.audit.record(
            "PackageService.Import",
            serde_json::json!({ "digest": digest, "bytes": bytes.len() }),
        );
        let cache = self.cache_dir.clone();
        let job = self
            .engine
            .submit("package.import", move |p| async move {
                let d = digest.clone();
                let file = tokio::task::spawn_blocking(move || {
                    vxedge_core::distribution::import(&bytes, &d, &cache)
                })
                .await
                .map_err(|e| format!("import task panicked: {e}"))?
                .map_err(|e| e.to_string())?;
                p.artifact(&file, digest.trim_start_matches("sha256:"));
                Ok::<_, crate::jobs::JobFailure>(format!("package {digest} in the cache"))
            })
            .await;
        Ok(Response::new(job))
    }
}
