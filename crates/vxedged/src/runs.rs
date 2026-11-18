//! `RunService`: the provisioned workspaces on this host, as an agent addresses them.
//!
//! A run is exactly as long-lived as its `active.json`: the gateway's `up` writes it and its `down`
//! removes it, so a run survives a repeated `up` and a daemon restart. The host runs several at
//! once, each in its own instance slot (`ARCHITECTURE.md` §6.0.5).
//!
//! A run's state is observed, not remembered: `UP` only when the gateway and every node it
//! provisions are up. A half-built run, for example after a daemon stopped in the middle of `up`,
//! reads `DEGRADED` until an `up` or `down` converges it, and `STAGING` while one acts on it.
//!
//! `Status` and `Down` are the fleet view (Track F): every run's status in one call, and several
//! runs, or every run, taken down as one job with a child job per run.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tonic::{Request, Response, Status};

use vxedge_api::v1::experiment_service_server::ExperimentService;
use vxedge_api::v1::gateway_service_server::GatewayService;
use vxedge_api::v1::manifest_ref::Source;
use vxedge_api::v1::run_service_server::RunService;
use vxedge_api::v1::vehicle_service_server::VehicleService;
use vxedge_api::v1::{
    ExperimentDownRequest, ExperimentStatusRequest, GatewayStatus, GatewayStatusRequest, Job,
    Kernel, ManifestRef, NodeStatus, PackageRef, ResourceKind, ResourceState, ResourceStatus, Run,
    RunDownRequest, RunGetRequest, RunListRequest, RunListResponse, RunRef, RunStatus,
    RunStatusRequest, RunStatusResponse, VehicleDownRequest, VehicleStatusRequest,
};
use vxedge_core::instance::Instance;

use crate::errors::R;
use crate::experiment::{ExperimentSvc, await_job};
use crate::jobs::{JobEngine, JobFailure, JobOpts, Progress, stage};
use crate::services::{ActiveRecord, GatewaySvc, read_workspace};
use crate::vehicle::VehicleSvc;

/// The run's state from what is observed. `STAGING` while a lifecycle job acts on it; the
/// gateway's own state unless it is up; then the workspace's status, which rolls up every zone,
/// compute node, service and signal plane it declares, so a half-built run reads `DEGRADED`.
pub(crate) fn run_state(gateway: i32, workspace: Option<i32>, busy: bool) -> i32 {
    if busy {
        return ResourceState::Staging as i32;
    }
    if gateway != ResourceState::Up as i32 {
        return gateway;
    }
    // A status that cannot be read leaves the gateway's state: the run exists either way.
    workspace.unwrap_or(gateway)
}

/// Whether the lifecycle job running now acts on the run in `dir`. A job that names no run, such
/// as a `run.down` over several, acts on every run.
pub(crate) fn acts_on(running: bool, claimed: Option<&Path>, dir: &Path) -> bool {
    running && claimed.is_none_or(|c| c == dir)
}

/// Whether the workspace is a bare vehicle, and the id its author wrote. A vehicle's record
/// holds the id of its synthesized experiment.
pub(crate) async fn target(record: &ActiveRecord) -> (ResourceKind, String) {
    let experiment = (ResourceKind::Experiment, record.id.clone());
    if record.dir == Path::new(".") {
        return experiment;
    }
    let Ok(yaml) = read_workspace(&record.dir.display().to_string()).await else {
        return experiment;
    };
    match vxedge_core::Workspace::from_yaml(&yaml) {
        Ok(ws) if ws.experiments.is_empty() => match ws.vehicles.first() {
            Some(vehicle) => (ResourceKind::Vehicle, vehicle.id.clone()),
            None => experiment,
        },
        _ => experiment,
    }
}

/// The runs a `Status` reports: the named ones that are up, lowest slot first, and the ids of
/// the named ones that are not, in the order they were named. No names means every run.
pub(crate) fn status_targets(
    recorded: Vec<(Instance, ActiveRecord)>,
    wanted: &[String],
) -> (Vec<(Instance, ActiveRecord)>, Vec<String>) {
    if wanted.is_empty() {
        return (recorded, Vec::new());
    }
    let mut missing: Vec<String> = Vec::new();
    for w in wanted {
        if !recorded.iter().any(|(_, r)| r.run_id() == w) && !missing.contains(w) {
            missing.push(w.clone());
        }
    }
    let found = recorded
        .into_iter()
        .filter(|(_, r)| wanted.iter().any(|w| w == r.run_id()))
        .collect();
    (found, missing)
}

/// Why a named run is absent: nothing is up, or other runs are.
fn absent(none_up: bool) -> R {
    if none_up {
        R::NoActiveRun
    } else {
        R::RunMismatch
    }
}

/// The run ids a request names. An empty id names nothing, so it is refused rather than read as
/// "every run".
fn named(runs: Vec<RunRef>) -> Result<Vec<String>, Status> {
    runs.into_iter()
        .map(|r| {
            if r.run_id.is_empty() {
                Err(crate::errors::invalid(
                    R::InvalidRequest,
                    "a `RunRef` in `runs` has no `run_id`",
                ))
            } else {
                Ok(r.run_id)
            }
        })
        .collect()
}

/// The runs a `Down` takes, lowest slot first, or why the call is refused. `reserved` names the
/// campaign that holds a slot between its trials.
pub(crate) fn down_targets(
    recorded: Vec<(Instance, ActiveRecord)>,
    wanted: &[String],
    all: bool,
    reserved: impl Fn(Instance) -> Option<String>,
) -> Result<Vec<(Instance, ActiveRecord)>, Status> {
    if all == !wanted.is_empty() {
        return Err(crate::errors::invalid(
            R::InvalidRequest,
            if all {
                "set `runs` or `all`, not both"
            } else {
                "name the runs to take down, or set `all`"
            },
        ));
    }
    let none_up = recorded.is_empty();
    let targets = if all {
        recorded
    } else {
        let (found, missing) = status_targets(recorded, wanted);
        if let Some(m) = missing.first() {
            let message = if none_up {
                format!("no run is up on this host, so there is no run `{m}`")
            } else {
                format!("no run `{m}` on this host")
            };
            return Err(crate::errors::not_found(absent(none_up), message));
        }
        found
    };
    // With `all`, a campaign between trials holds a slot with no run in it yet, and would bring
    // its next trial up there after this call reports the host cleared: it counts too.
    let slots: Vec<Instance> = if all {
        Instance::all().collect()
    } else {
        targets.iter().map(|(i, _)| *i).collect()
    };
    let held: Vec<String> = slots
        .into_iter()
        .filter_map(|i| reserved(i).map(|c| format!("slot {i} (campaign `{c}`)")))
        .collect();
    if !held.is_empty() {
        return Err(crate::errors::precondition(
            R::HeldByRun,
            format!(
                "held by a running campaign between its trials: {}. Cancel the campaign or wait \
                 for it to end; nothing was taken down",
                held.join(", ")
            ),
        ));
    }
    Ok(targets)
}

/// What one read of a run observed: the gateway, and the workspace's status when it could be
/// read.
struct Observed {
    gateway_state: i32,
    gateway: Option<GatewayStatus>,
    workspace: Result<WorkspaceRead, Status>,
}

struct WorkspaceRead {
    status: Option<ResourceStatus>,
    components: Vec<ResourceStatus>,
    nodes: Vec<NodeStatus>,
}

pub struct RunSvc {
    pub gateway: Arc<GatewaySvc>,
    pub experiments: Arc<ExperimentSvc>,
    pub vehicles: Arc<VehicleSvc>,
    pub runtime_dir: PathBuf,
}

impl RunSvc {
    /// The gateway and the workspace, from one workspace status read: vehicle and experiment status
    /// both carry the gateway, so it is not observed twice.
    async fn observe(&self, record: &ActiveRecord, kind: ResourceKind) -> Result<Observed, Status> {
        let run_ref = RunRef {
            run_id: record.run_id().to_string(),
        };
        let workspace = if record.dir == Path::new(".") {
            Err(crate::errors::precondition(
                R::WorkspaceInvalid,
                "the run was brought up from an inline manifest, so its workspace cannot be read",
            ))
        } else {
            let manifest = Some(ManifestRef {
                source: Some(Source::Path(record.dir.display().to_string())),
                ..Default::default()
            });
            match kind {
                ResourceKind::Vehicle => self
                    .vehicles
                    .status(Request::new(VehicleStatusRequest {
                        run: Some(run_ref.clone()),
                        manifest,
                    }))
                    .await
                    .map(|r| {
                        let r = r.into_inner();
                        (r.gateway, r.status, r.components, r.nodes)
                    }),
                _ => self
                    .experiments
                    .status(Request::new(ExperimentStatusRequest {
                        run: Some(run_ref.clone()),
                        manifest,
                    }))
                    .await
                    .map(|r| {
                        let r = r.into_inner();
                        (r.gateway, r.status, r.components, r.nodes)
                    }),
            }
        };
        let (gateway, workspace) = match workspace {
            Ok((gateway, status, components, nodes)) => (
                gateway,
                Ok(WorkspaceRead {
                    status,
                    components,
                    nodes,
                }),
            ),
            Err(e) => (None, Err(e)),
        };
        let gateway = match gateway {
            Some(g) if g.status.is_some() => g,
            _ => self
                .gateway
                .status(Request::new(GatewayStatusRequest { run: Some(run_ref) }))
                .await?
                .into_inner(),
        };
        Ok(Observed {
            gateway_state: gateway.status.as_ref().map_or(0, |s| s.state),
            gateway: Some(gateway),
            workspace,
        })
    }

    /// Every run on the host, lowest slot first.
    async fn all(&self) -> Result<Vec<Run>, Status> {
        let mut runs = Vec::new();
        for (instance, record) in crate::scope::recorded(&self.runtime_dir) {
            runs.push(self.read(instance, record, false).await?.0);
        }
        Ok(runs)
    }

    /// Whether a lifecycle job acts on the run in `instance` now.
    async fn staging(&self, instance: Instance) -> bool {
        let engine = &self.gateway.engine;
        let running = engine.lifecycle_running().await;
        let claimed = engine.claimed_run_dir().await;
        acts_on(
            running,
            claimed.as_deref(),
            &crate::scope::run_dir(&self.runtime_dir, instance),
        )
    }

    /// A run, and what was observed of it. While a job acts on the run its state is `STAGING`
    /// whatever is observed, so `List` and `Get` skip the observation, the slow part of the call;
    /// `Status` observes anyway (`always`), because the detail is what it is asked for.
    async fn read(
        &self,
        instance: Instance,
        record: ActiveRecord,
        always: bool,
    ) -> Result<(Run, Option<Observed>), Status> {
        let (kind, target_id) = target(&record).await;
        let busy = self.staging(instance).await;
        let observed = if busy && !always {
            None
        } else {
            Some(self.observe(&record, kind).await?)
        };
        let gateway_state = observed
            .as_ref()
            .map_or(ResourceState::Staging as i32, |o| o.gateway_state);
        let workspace = observed
            .as_ref()
            .and_then(|o| o.workspace.as_ref().ok())
            .and_then(|w| w.status.as_ref())
            .map(|s| s.state);
        let run = Run {
            instance: instance.to_string(),
            id: record.run_id().to_string(),
            kind: kind as i32,
            target_id,
            workspace_identity_algorithm: record.identity_algorithm().to_string(),
            package_locked: record.package.as_ref().is_some_and(|p| p.locked),
            package: record.package.map(|p| PackageRef {
                id: p.id,
                version: p.version,
                digest: p.digest,
            }),
            kernel: record.kernel.map(|k| Kernel {
                release: k.release,
                preempt_rt: k.preempt_rt,
            }),
            shaping: record.shaping.unwrap_or_default(),
            time_sync: record.time_sync.unwrap_or_default(),
            workspace_sha256: record.sha256.unwrap_or_default(),
            state: run_state(gateway_state, workspace, busy),
            started_unix_ms: record.started_unix_ms.unwrap_or_default(),
        };
        Ok((run, observed))
    }

    /// One run's `RunStatus`. A read that fails becomes the entry's `error_reason`, never the
    /// call's.
    async fn status_of(&self, instance: Instance, record: ActiveRecord) -> RunStatus {
        let bare = Run {
            id: record.run_id().to_string(),
            instance: instance.to_string(),
            ..Default::default()
        };
        let failed = |run: Run, e: &Status| RunStatus {
            run: Some(run),
            error_reason: JobFailure::from_status(e).reason as i32,
            error_message: e.message().to_string(),
            ..Default::default()
        };
        match self.read(instance, record, true).await {
            Err(e) => failed(bare, &e),
            Ok((run, None)) => RunStatus {
                run: Some(run),
                ..Default::default()
            },
            Ok((run, Some(o))) => match o.workspace {
                Ok(w) => RunStatus {
                    run: Some(run),
                    status: w.status,
                    components: w.components,
                    gateway: o.gateway,
                    nodes: w.nodes,
                    ..Default::default()
                },
                Err(e) => RunStatus {
                    gateway: o.gateway,
                    ..failed(run, &e)
                },
            },
        }
    }
}

/// Start one run's own `down` as a stage of `parent`, in its slot. A `down` that cannot start
/// still leaves a failed child job carrying its reason, so every run the parent takes has one.
async fn down_one(
    experiments: &Arc<ExperimentSvc>,
    vehicles: &Arc<VehicleSvc>,
    engine: &JobEngine,
    root: &Path,
    parent: &str,
    instance: Instance,
    record: &ActiveRecord,
) -> Job {
    let (kind, _) = target(record).await;
    let run = Some(RunRef {
        run_id: record.run_id().to_string(),
    });
    let job_kind = match kind {
        ResourceKind::Vehicle => "vehicle.down",
        _ => "experiment.down",
    };
    let started = if record.dir == Path::new(".") {
        Err(crate::errors::precondition(
            R::WorkspaceInvalid,
            format!(
                "run `{}` was brought up from an inline manifest; take it down with \
                 `ExperimentService.Down` and that manifest",
                record.run_id()
            ),
        ))
    } else {
        let manifest = Some(ManifestRef {
            source: Some(Source::Path(record.dir.display().to_string())),
            ..Default::default()
        });
        match kind {
            ResourceKind::Vehicle => VehicleService::down(
                &**vehicles,
                crate::scope::with_slot(
                    stage(
                        VehicleDownRequest {
                            manifest,
                            run,
                            ..Default::default()
                        },
                        parent,
                    ),
                    instance,
                ),
            )
            .await
            .map(Response::into_inner),
            _ => ExperimentService::down(
                &**experiments,
                crate::scope::with_slot(
                    stage(
                        ExperimentDownRequest {
                            manifest,
                            run,
                            ..Default::default()
                        },
                        parent,
                    ),
                    instance,
                ),
            )
            .await
            .map(Response::into_inner),
        }
    };
    match started {
        Ok(job) => job,
        Err(e) => {
            let failure = JobFailure::from_status(&e);
            engine
                .submit_with(
                    JobOpts::lifecycle(&stage((), parent), job_kind)
                        .in_run(&crate::scope::run_dir(root, instance)),
                    job_kind,
                    move |_| async move { Err::<String, _>(failure) },
                )
                .await
                .expect("a stage takes no claim and has no request id, so it is never refused")
        }
    }
}

/// Take `targets` down one after another, each as a child job, best effort: a run that fails
/// does not stop the others. The first failure's reason is the parent's.
async fn down_all(
    experiments: Arc<ExperimentSvc>,
    vehicles: Arc<VehicleSvc>,
    engine: JobEngine,
    root: PathBuf,
    targets: Vec<(Instance, ActiveRecord)>,
    p: Progress,
) -> Result<String, JobFailure> {
    let n = targets.len();
    if n == 0 {
        return Ok("no run is up".to_string());
    }
    let parent = p.job_id().to_string();
    let mut first: Option<JobFailure> = None;
    let mut failed = 0;
    for (k, (instance, record)) in targets.into_iter().enumerate() {
        p.checkpoint()?;
        let percent = u32::try_from(k * 100 / n).unwrap_or(99);
        let label = format!("run `{}` (slot {instance})", record.run_id());
        p.update(percent, &format!("{label}: down"));
        let child = down_one(
            &experiments,
            &vehicles,
            &engine,
            &root,
            &parent,
            instance,
            &record,
        )
        .await;
        if let Err(e) = await_job(&engine, child, &p, percent, &label).await {
            failed += 1;
            first.get_or_insert(e);
        }
    }
    match first {
        None => Ok(format!("{n} run(s) down")),
        Some(e) => Err(JobFailure::new(
            e.reason,
            format!(
                "{failed} of {n} run(s) did not go down; the first: {}",
                e.message
            ),
        )),
    }
}

#[tonic::async_trait]
impl RunService for RunSvc {
    async fn get(&self, req: Request<RunGetRequest>) -> Result<Response<Run>, Status> {
        let wanted = req.into_inner().run.map(|r| r.run_id).unwrap_or_default();
        let runs = self.all().await?;
        if runs.is_empty() {
            return Err(crate::errors::not_found(
                R::NoActiveRun,
                "no run is active on this host",
            ));
        }
        if wanted.is_empty() {
            return match <[Run; 1]>::try_from(runs) {
                Ok([run]) => Ok(Response::new(run)),
                Err(runs) => Err(crate::errors::precondition(
                    R::RunAmbiguous,
                    format!(
                        "{} runs are up, so name one: {}",
                        runs.len(),
                        runs.iter()
                            .map(|r| format!("`{}` (slot {})", r.id, r.instance))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )),
            };
        }
        runs.into_iter()
            .find(|r| r.id == wanted)
            .map(Response::new)
            .ok_or_else(|| {
                crate::errors::not_found(R::RunMismatch, format!("no run `{wanted}` on this host"))
            })
    }

    async fn list(&self, _: Request<RunListRequest>) -> Result<Response<RunListResponse>, Status> {
        let runs = self.all().await?;
        Ok(Response::new(RunListResponse { runs }))
    }

    async fn status(
        &self,
        req: Request<RunStatusRequest>,
    ) -> Result<Response<RunStatusResponse>, Status> {
        let wanted = named(req.into_inner().runs)?;
        let recorded = crate::scope::recorded(&self.runtime_dir);
        let none_up = recorded.is_empty();
        let (found, missing) = status_targets(recorded, &wanted);
        let mut runs = Vec::with_capacity(found.len() + missing.len());
        for (instance, record) in found {
            runs.push(self.status_of(instance, record).await);
        }
        let reason = absent(none_up);
        runs.extend(missing.into_iter().map(|id| RunStatus {
            error_message: format!("no run `{id}` on this host"),
            run: Some(Run {
                id,
                ..Default::default()
            }),
            error_reason: reason as i32,
            ..Default::default()
        }));
        Ok(Response::new(RunStatusResponse { runs }))
    }

    async fn down(&self, req: Request<RunDownRequest>) -> Result<Response<Job>, Status> {
        let all = req.get_ref().all;
        let wanted = named(req.get_ref().runs.clone())?;
        let root = self.runtime_dir.clone();
        let targets = down_targets(crate::scope::recorded(&root), &wanted, all, |i| {
            crate::scope::reserved_by(&root, i)
        })?;
        let ids: Vec<String> = targets
            .iter()
            .map(|(_, r)| r.run_id().to_string())
            .collect();
        self.gateway.audit.record(
            "RunService.Down",
            serde_json::json!({ "runs": ids, "all": all }),
        );
        let claim = if all {
            "every run".to_string()
        } else {
            format!("runs {}", ids.join(","))
        };
        let (experiments, vehicles) = (self.experiments.clone(), self.vehicles.clone());
        let engine = self.gateway.engine.clone();
        let job = self
            .gateway
            .engine
            .submit_with(JobOpts::lifecycle(&req, claim), "run.down", move |p| {
                down_all(experiments, vehicles, engine, root, targets, p)
            })
            .await?;
        Ok(Response::new(job))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic_types::StatusExt;

    fn slot(c: &str) -> Instance {
        Instance::parse(c).unwrap()
    }

    fn record(run_id: &str) -> ActiveRecord {
        ActiveRecord {
            id: "car".into(),
            dir: format!("/ws/{run_id}").into(),
            bridge: "vxbr0".into(),
            sha256: None,
            identity_algorithm: None,
            run_id: Some(run_id.into()),
            started_unix_ms: None,
            package: None,
            kernel: None,
            shaping: None,
            time_sync: None,
        }
    }

    fn runs() -> Vec<(Instance, ActiveRecord)> {
        vec![
            (slot("0"), record("run-a")),
            (slot("1"), record("run-b")),
            (slot("2"), record("run-c")),
        ]
    }

    fn ids(t: &[(Instance, ActiveRecord)]) -> Vec<&str> {
        t.iter().map(|(_, r)| r.run_id()).collect()
    }

    fn reason(s: &Status) -> String {
        s.get_error_details()
            .error_info()
            .map(|i| i.reason.clone())
            .unwrap_or_default()
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn status_reports_every_run_or_the_named_ones_and_lists_the_absent() {
        let (found, missing) = status_targets(runs(), &[]);
        assert_eq!(ids(&found), ["run-a", "run-b", "run-c"]);
        assert!(missing.is_empty());

        let (found, missing) = status_targets(runs(), &names(&["run-c", "gone", "run-a", "gone"]));
        assert_eq!(ids(&found), ["run-a", "run-c"], "lowest slot first");
        assert_eq!(
            missing,
            ["gone"],
            "each absent run once, in the order named"
        );
    }

    #[test]
    fn down_takes_exactly_one_of_runs_or_all() {
        let none = |_: Instance| None;
        let both = down_targets(runs(), &names(&["run-a"]), true, none).unwrap_err();
        assert_eq!(reason(&both), "ERROR_REASON_INVALID_REQUEST");
        let neither = down_targets(runs(), &[], false, none).unwrap_err();
        assert_eq!(reason(&neither), "ERROR_REASON_INVALID_REQUEST");
        assert_eq!(
            ids(&down_targets(runs(), &[], true, none).unwrap()),
            ["run-a", "run-b", "run-c"]
        );
        assert_eq!(
            ids(&down_targets(runs(), &names(&["run-b"]), false, none).unwrap()),
            ["run-b"]
        );
        assert!(
            down_targets(Vec::new(), &[], true, none)
                .unwrap()
                .is_empty(),
            "every run, with nothing up, is nothing to do"
        );
    }

    #[test]
    fn down_refuses_a_named_run_that_is_not_up() {
        let none = |_: Instance| None;
        let e = down_targets(runs(), &names(&["run-a", "gone"]), false, none).unwrap_err();
        assert_eq!(reason(&e), "ERROR_REASON_RUN_MISMATCH");
        let e = down_targets(Vec::new(), &names(&["gone"]), false, none).unwrap_err();
        assert_eq!(reason(&e), "ERROR_REASON_NO_ACTIVE_RUN");
    }

    #[test]
    fn down_refuses_the_whole_call_when_a_campaign_holds_a_slot() {
        let held = |i: Instance| (i == slot("1") || i == slot("5")).then(|| format!("c-{i}"));
        let e = down_targets(runs(), &[], true, held).unwrap_err();
        assert_eq!(reason(&e), "ERROR_REASON_HELD_BY_RUN");
        assert!(
            e.message().contains("slot 1 (campaign `c-1`)")
                && e.message().contains("slot 5 (campaign `c-5`)"),
            "every held slot is named, a campaign's empty slot included: {}",
            e.message()
        );
        let e = down_targets(runs(), &names(&["run-a", "run-b"]), false, held).unwrap_err();
        assert!(e.message().contains("slot 1") && !e.message().contains("slot 5"));
        assert_eq!(
            ids(&down_targets(runs(), &names(&["run-c"]), false, held).unwrap()),
            ["run-c"],
            "a run whose slot no campaign holds goes down"
        );
    }

    #[test]
    fn down_refuses_a_run_ref_with_no_id() {
        let e = named(vec![RunRef::default()]).unwrap_err();
        assert_eq!(reason(&e), "ERROR_REASON_INVALID_REQUEST");
    }

    #[test]
    fn only_the_run_a_lifecycle_job_acts_on_is_staging() {
        let a = Path::new("/rt/runs/0");
        let b = Path::new("/rt/runs/1");
        assert!(acts_on(true, Some(a), a));
        assert!(
            !acts_on(true, Some(a), b),
            "another run is observed as usual"
        );
        assert!(
            acts_on(true, None, b),
            "a job over several runs acts on each"
        );
        assert!(!acts_on(false, None, a));
    }

    #[test]
    fn a_run_reads_the_workspace_rollup_once_the_gateway_is_up() {
        let up = ResourceState::Up as i32;
        let degraded = ResourceState::Degraded as i32;
        assert_eq!(run_state(up, Some(up), false), up);
        assert_eq!(
            run_state(up, Some(degraded), false),
            degraded,
            "a half-built run is not up"
        );
        assert_eq!(
            run_state(up, Some(degraded), true),
            ResourceState::Staging as i32,
            "a run with a lifecycle job in flight is staging"
        );
        let down = ResourceState::Down as i32;
        assert_eq!(run_state(down, Some(up), false), down);
        assert_eq!(
            run_state(up, None, false),
            up,
            "no readable status keeps the gateway's"
        );
    }
}
