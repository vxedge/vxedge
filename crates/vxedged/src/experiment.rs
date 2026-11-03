//! `ExperimentService` — one verb that brings a whole experiment up or down.
//!
//! The RPC has been in `proto/vxedge/v1/lifecycle.proto` since phase 1 with no implementation
//! behind it; campaign scripts did the sequencing themselves, calling `vxedge up`, then driving
//! probes and load generators directly. This moves that sequencing into the daemon, where the
//! hard rule says orchestration belongs.
//!
//! # What `up` does, and what it deliberately does not
//!
//! `up` provisions the gateway (including the 5G core, when the manifest declares `s-nssai`
//! slices), then every zone, then the declared services. **It does not start traffic**, and that
//! is a design decision rather than an omission.
//!
//! `up` is required to be idempotent and reconciling — running it twice is a no-op (AGENTS.md).
//! Traffic generation is not a converged state; it is a time-bounded process with a duration.
//! "Start a 60-second load run" cannot be made idempotent, so it does not belong inside a
//! reconciling verb. It stays `vxedge traffic up`, which already exists.
//!
//! The same split is what a measurement campaign needs anyway: exp-001 sweeps offered load
//! across arms over a topology that must be identical each time. Provisioning and load are
//! different lifetimes, and collapsing them would make the sweep impossible to express.
//!
//! # Composition, not reimplementation
//!
//! Each stage submits the *existing* service's job and waits for it. Nothing about gateway,
//! zone or service provisioning is duplicated here, so a fix in one of them is a fix here too.
//! The sub-jobs stay individually visible through `JobService`, which makes a failure
//! attributable to a stage rather than to "the experiment".

use std::sync::Arc;
use tonic::{Request, Response, Status};

use vxedge_api::v1::compute_service_server::ComputeService;
use vxedge_api::v1::experiment_service_server::ExperimentService;
use vxedge_api::v1::gateway_service_server::GatewayService;
use vxedge_api::v1::svc_service_server::SvcService;
use vxedge_api::v1::zone_service_server::ZoneService;
use vxedge_api::v1::{
    ComputeDownRequest, ComputeListRequest, ComputeUpRequest, ExperimentDownRequest,
    ExperimentStage, ExperimentStatus, ExperimentStatusRequest, ExperimentUpRequest,
    GatewayDownRequest, GatewayStatusRequest, GatewayUpRequest, Job, JobState, ManifestRef,
    ResourceKind, ResourceStatus, SvcDownRequest, SvcUpRequest, VxidRef, ZoneDownRequest,
    ZoneListRequest, ZoneUpRequest, manifest_ref::Source,
};

use std::path::Path;

use vxedge_core::containers::ContainerSpec;
use vxedge_core::gateway::GatewayPlan;
use vxedge_core::services::KUKSA_MEMBER;
use vxedge_core::zones::NodePlan;
use vxedge_core::zones::backend::NodeState;

use crate::audit::Audit;
use crate::events::EventKind;
use crate::jobs::{
    JobEngine, JobFailure, JobOpts, Progress, START_WINDOW, is_terminal, stage, start_then_confirm,
};
use crate::services::{ComputeSvc, GatewaySvc, SvcSvc, ZoneSvc};
use vxedge_api::v1::ErrorReason;

pub struct ExperimentSvc {
    pub audit: Arc<Audit>,
    pub handles: Handles,
}

/// The sub-services a stage drives, in a form a job body can own.
///
/// `JobEngine::submit` takes a `'static` future, so the stages cannot borrow from `&self`.
/// `Arc` per service rather than cloning the services themselves: they hold the shared
/// `EdgeState`, and duplicating the struct around it would invite two handles that look
/// independent but are not.
#[derive(Clone)]
pub struct Handles {
    pub engine: JobEngine,
    pub gateway: Arc<GatewaySvc>,
    pub zones: Arc<ZoneSvc>,
    pub compute: Arc<ComputeSvc>,
    pub svcs: Arc<SvcSvc>,
}

/// Which service addresses a node: `ComputeService` for an hpc device, `ZoneService` otherwise.
/// They provision the same way and differ in what lands in the audit log and the event stream.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeStage {
    Zones,
    Compute,
}

/// Wait for a submitted sub-job to reach a terminal state, relaying its progress to the parent.
///
/// Returns the job's own message on success and its error on failure, so the experiment job's
/// failure text names what actually broke instead of "stage 2 failed". The relay is what makes
/// `JobService.Watch` on a composite job show each step rather than one line per stage.
async fn await_job(
    engine: &JobEngine,
    job: Job,
    parent: &Progress,
    percent: u32,
    label: &str,
) -> Result<String, JobFailure> {
    let mut rx = engine.watch(&job.id).await.ok_or_else(|| {
        JobFailure::from(format!(
            "job {} vanished before it could be watched",
            job.id
        ))
    })?;
    let mut relayed = String::new();
    loop {
        {
            let current = rx.borrow_and_update();
            if !current.message.is_empty() && current.message != relayed {
                relayed.clone_from(&current.message);
                parent.update(percent, &format!("{label}: {relayed}"));
            }
            if is_terminal(current.state) {
                return if current.state == JobState::Succeeded as i32 {
                    Ok(current.message.clone())
                } else if current.state == JobState::Cancelled as i32 {
                    Err(JobFailure::cancelled())
                } else {
                    Err(JobFailure::new(
                        ErrorReason::try_from(current.error_reason).unwrap_or_default(),
                        current.error.clone(),
                    ))
                };
            }
        }
        if rx.changed().await.is_err() {
            return Err(JobFailure::from(format!(
                "job {} stopped reporting before finishing",
                job.id
            )));
        }
    }
}

/// `dir` is the workspace `resolve_once` (or the caller's own preflight resolution) already
/// read this document from — carried alongside the YAML so a sub-RPC can resolve local
/// bundle/part refs against the real workspace instead of the daemon's own cwd (see
/// `ManifestRef.resolved_dir`'s own doc comment, common.proto). `None` for a genuinely-inline
/// manifest (no workspace to name).
/// Refuse a verb aimed at anything but what is active on this host.
///
/// The host has one gateway, so without this a Down naming workspace B tore down whatever A
/// someone else had up, and an Up naming B took A's bridge and left A's nodes running without
/// it. Matched on id, and on the workspace directory too when both sides know it — two copies
/// of one workspace in different directories are different runs. `"."` is what `gateway up`
/// records for a genuinely inline manifest, i.e. no known directory. Nothing active means
/// nothing to protect: the verb proceeds, which keeps `down` idempotent.
pub(crate) fn check_active_target(
    verb: &str,
    active: Option<(&str, &std::path::Path)>,
    requested_id: &str,
    requested_dir: Option<&std::path::Path>,
) -> Result<(), Status> {
    let Some((active_id, active_dir)) = active else {
        return Ok(());
    };
    let active_dir_known = active_dir != std::path::Path::new(".");
    let same_dir = match requested_dir {
        Some(dir) if active_dir_known => dir == active_dir,
        _ => true,
    };
    if active_id == requested_id && same_dir {
        return Ok(());
    }
    let requested_at = requested_dir
        .map(|d| format!(" (workspace {})", d.display()))
        .unwrap_or_default();
    let active_at = if active_dir_known {
        format!(" (workspace {})", active_dir.display())
    } else {
        String::new()
    };
    Err(crate::errors::precondition(
        crate::errors::R::WrongWorkspace,
        format!(
            "`{active_id}`{active_at} is the one active on this host, not \
         `{requested_id}`{requested_at}: `{verb}` only acts on what is active — run it from that \
         workspace, or bring it down first"
        ),
    ))
}

pub(crate) fn inline(yaml: &str, dir: Option<&std::path::Path>) -> Option<ManifestRef> {
    Some(ManifestRef {
        source: Some(Source::InlineYaml(yaml.to_string())),
        sha256: String::new(),
        resolved_dir: dir.map(|d| d.display().to_string()).unwrap_or_default(),
    })
}

/// Resolve the request's manifest ref to YAML once, then hand the *same* document to every
/// stage as `inline_yaml`.
///
/// Re-resolving per stage would re-read the workspace from disk each time, so an edit made
/// mid-run could leave the gateway provisioned from one revision and the zones from another.
/// One read, one document, one experiment.
async fn resolve_once(mref: Option<&ManifestRef>) -> Result<String, Status> {
    let mref = mref.ok_or_else(|| {
        crate::errors::invalid(
            crate::errors::R::InvalidRequest,
            "no experiment to run: the daemon received no manifest or workspace reference",
        )
    })?;
    crate::services::resolve_manifest_yaml(mref).await
}

impl Handles {
    pub(crate) async fn stage_gateway(
        &self,
        yaml: &str,
        dir: Option<&std::path::Path>,
        up: bool,
        p: &Progress,
    ) -> Result<String, JobFailure> {
        p.update(
            10,
            if up {
                "gateway: bridge, slices, endpoints"
            } else {
                "gateway: removing bridge, slices, endpoints"
            },
        );
        let job = if up {
            self.gateway
                .up(stage(
                    GatewayUpRequest {
                        manifest: inline(yaml, dir),
                    },
                    p.job_id(),
                ))
                .await
        } else {
            self.gateway
                .down(stage(
                    GatewayDownRequest {
                        manifest: inline(yaml, dir),
                    },
                    p.job_id(),
                ))
                .await
        }
        .map_err(|e| JobFailure::from_status(&e).context("gateway"))?;
        await_job(&self.engine, job.into_inner(), p, 30, "gateway")
            .await
            .map_err(|e| e.context("gateway"))
    }

    /// Bring every provisioned device up or down, one RPC each.
    ///
    /// The list comes from `ZoneService` rather than from the workspace, because what it can
    /// plan *is* what has to be staged: a device with no VLAN attachment has no bridge port and
    /// no node plan, and asking for it by name would fail the stage on a device that was never
    /// meant to be provisioned. Addressing devices individually rather than by region also
    /// keeps `up` from starting several guests concurrently in one call — the concurrency that
    /// invalidated exp-002b's first campaign.
    pub(crate) async fn stage_zones(&self, up: bool, p: &Progress) -> Result<String, JobFailure> {
        let listed = match self
            .zones
            .list(Request::new(ZoneListRequest::default()))
            .await
        {
            Ok(r) => r.into_inner().zones,
            // A plan that cannot be made is a broken workspace, and reading it as "no zones"
            // reported success with nothing provisioned.
            Err(e) if e.code() == tonic::Code::Internal => {
                return Err(JobFailure::from_status(&e).context("zones"));
            }
            // No active experiment, or nothing plannable in it: both mean there is nothing to
            // stage, which is not a failure.
            Err(_) => return Ok("no zones declared".into()),
        };
        if listed.is_empty() {
            return Ok("no zones declared".into());
        }
        let ids: Vec<String> = listed.into_iter().map(|n| n.vxid).collect();
        self.stage_nodes(NodeStage::Zones, &ids, up, p).await
    }

    /// Bring every compute device up or down, listed the way [`Self::stage_zones`] lists zones.
    pub(crate) async fn stage_compute(&self, up: bool, p: &Progress) -> Result<String, JobFailure> {
        let request = Request::new(ComputeListRequest::default());
        let listed = match self.compute.list(request).await {
            Ok(r) => r.into_inner().nodes,
            Err(e) if e.code() == tonic::Code::Internal => {
                return Err(JobFailure::from_status(&e).context("compute"));
            }
            Err(_) => return Ok("no compute declared".into()),
        };
        if listed.is_empty() {
            return Ok("no compute declared".into());
        }
        let ids: Vec<String> = listed.into_iter().map(|n| n.vxid).collect();
        self.stage_nodes(NodeStage::Compute, &ids, up, p).await
    }

    /// Bring these nodes up or down, one RPC each. `up` stops at the first failure. `down` tries
    /// every node and reports them all, because stopping would strand the ones after it.
    pub(crate) async fn stage_nodes(
        &self,
        stage: NodeStage,
        ids: &[String],
        up: bool,
        p: &Progress,
    ) -> Result<String, JobFailure> {
        let total = ids.len();
        let (label, base, span) = match stage {
            NodeStage::Zones => ("zone", 40, 20),
            NodeStage::Compute => ("compute", 60, 15),
        };
        let mut failures = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            p.checkpoint()?;
            let percent = base + (i as u32 * span) / total as u32;
            let verb = if up { "up" } else { "down" };
            p.update(
                percent,
                &format!("{label} {}/{total}: {id} ({verb})", i + 1),
            );
            let step = format!("{label} {id}");
            let result = match self.submit_node(stage, id, up, p.job_id()).await {
                Ok(job) => await_job(&self.engine, job, p, percent, &step).await,
                Err(status) => Err(JobFailure::from_status(&status)),
            };
            if let Err(e) = result {
                let failure = e.context(format!("{label} `{id}`"));
                if up {
                    return Err(failure);
                }
                failures.push(failure);
            }
        }
        if failures.is_empty() {
            Ok(format!("{total} {label}(s)"))
        } else {
            Err(JobFailure::joined(label, failures))
        }
    }

    async fn submit_node(
        &self,
        stage: NodeStage,
        id: &str,
        up: bool,
        parent: &str,
    ) -> Result<Job, Status> {
        let r#ref = Some(VxidRef {
            vxid: id.to_string(),
        });
        let job = match (stage, up) {
            (NodeStage::Zones, true) => {
                self.zones
                    .up(crate::jobs::stage(
                        ZoneUpRequest {
                            r#ref,
                            ..Default::default()
                        },
                        parent,
                    ))
                    .await
            }
            (NodeStage::Zones, false) => {
                self.zones
                    .down(crate::jobs::stage(
                        ZoneDownRequest {
                            r#ref,
                            ..Default::default()
                        },
                        parent,
                    ))
                    .await
            }
            (NodeStage::Compute, true) => {
                self.compute
                    .up(crate::jobs::stage(
                        ComputeUpRequest {
                            r#ref,
                            ..Default::default()
                        },
                        parent,
                    ))
                    .await
            }
            (NodeStage::Compute, false) => {
                self.compute
                    .down(crate::jobs::stage(
                        ComputeDownRequest {
                            r#ref,
                            ..Default::default()
                        },
                        parent,
                    ))
                    .await
            }
        }?;
        Ok(job.into_inner())
    }

    /// The Kuksa databroker, as one more member of its device's container group: the path a
    /// placed service takes. `vehicle up` and `experiment up` both run this, so a declared signal
    /// plane either starts or fails the job, and is never skipped.
    pub(crate) async fn stage_signal_plane(
        &self,
        model: &vxedge_core::model::Experiment,
        dir: Option<&Path>,
        up: bool,
        p: &Progress,
    ) -> Result<String, JobFailure> {
        let fail = |e: String| JobFailure::from(format!("signal plane `{KUKSA_MEMBER}`: {e}"));
        let Some((node, spec)) = signal_plane_member(model, dir).map_err(fail)? else {
            return Ok("no signal plane declared".into());
        };
        let on = node.node_id.clone();
        let verb = if up { "up" } else { "down" };
        p.update(
            72,
            &format!("signal plane: {KUKSA_MEMBER} on {on} ({verb})"),
        );
        self.svcs.audit.record(
            if up {
                "SignalPlane.Up"
            } else {
                "SignalPlane.Down"
            },
            serde_json::json!({ "member": KUKSA_MEMBER, "on": on, "image": spec.image }),
        );
        let shell = self.zones.shell.clone();
        let runtime_dir = self.zones.runtime_dir.clone();
        let (probe_node, probe_spec) = (node.clone(), spec.clone());
        let change = async move {
            tokio::task::spawn_blocking(move || {
                if up {
                    vxedge_core::zones::member_up(&shell, &node, &runtime_dir, &spec)
                } else {
                    vxedge_core::zones::member_down(&node, &spec)
                }
            })
            .await
            .map_err(|e| format!("task panicked: {e}"))?
            .map_err(|e| e.to_string())
        };
        if up {
            let running = || {
                let (node, spec) = (probe_node.clone(), probe_spec.clone());
                async move {
                    tokio::task::spawn_blocking(move || {
                        vxedge_core::zones::member_status(&node, &spec) == NodeState::Up
                    })
                    .await
                    .unwrap_or(false)
                }
            };
            start_then_confirm(change, running, START_WINDOW)
                .await
                .map_err(fail)?;
        } else {
            change.await.map_err(fail)?;
        }
        let (kind, msg) = if up {
            (
                EventKind::SignalPlaneUp,
                format!("signal plane up on `{on}`"),
            )
        } else {
            (
                EventKind::SignalPlaneDown,
                format!("signal plane stopped on `{on}`"),
            )
        };
        self.engine.events().emit(kind, KUKSA_MEMBER, &msg);
        Ok(msg)
    }

    /// The databroker as a status component, observed through Podman. `None` when none is
    /// declared.
    pub(crate) async fn signal_plane_component(
        &self,
        model: &vxedge_core::model::Experiment,
        dir: Option<&Path>,
    ) -> Result<Option<ResourceStatus>, Status> {
        let member = signal_plane_member(model, dir)
            .map_err(|e| crate::errors::precondition(crate::errors::R::WorkspaceInvalid, e))?;
        let Some((node, spec)) = member else {
            return Ok(None);
        };
        let on = node.node_id.clone();
        let image = spec.image.clone();
        let state =
            tokio::task::spawn_blocking(move || vxedge_core::zones::member_status(&node, &spec))
                .await
                .map_err(|e| Status::internal(format!("status task panicked: {e}")))?;
        Ok(Some(crate::status::resource_status(
            KUKSA_MEMBER,
            ResourceKind::SignalPlane,
            &state,
            format!("signal plane on `{on}`: {image}"),
        )))
    }

    pub(crate) async fn stage_services(
        &self,
        model: &vxedge_core::model::Experiment,
        yaml: &str,
        dir: Option<&std::path::Path>,
        up: bool,
        p: &Progress,
    ) -> Result<String, JobFailure> {
        // Keyed by service *type* because that is what SvcService dispatches on.
        let types = vxedge_core::services::ServiceDef::declared_types(model);
        if types.is_empty() {
            return Ok("no services declared".into());
        }
        for (i, ty) in types.iter().enumerate() {
            p.update(
                75 + (i as u32 * 20) / types.len() as u32,
                &format!("service {}: {}", ty, if up { "up" } else { "down" }),
            );
            let job = if up {
                self.svcs
                    .up(stage(
                        SvcUpRequest {
                            service_type: ty.clone(),
                            experiment: inline(yaml, dir),
                            ..Default::default()
                        },
                        p.job_id(),
                    ))
                    .await
            } else {
                self.svcs
                    .down(stage(
                        SvcDownRequest {
                            service_type: ty.clone(),
                            experiment: inline(yaml, dir),
                            ..Default::default()
                        },
                        p.job_id(),
                    ))
                    .await
            }
            .map_err(|e| JobFailure::from_status(&e).context(format!("service `{ty}`")))?;
            await_job(
                &self.engine,
                job.into_inner(),
                p,
                90,
                &format!("service {ty}"),
            )
            .await
            .map_err(|e| e.context(format!("service `{ty}`")))?;
        }
        Ok(format!("{} service(s)", types.len()))
    }
}

/// The databroker's node and container, if this workspace declares a signal plane.
fn signal_plane_member(
    model: &vxedge_core::model::Experiment,
    dir: Option<&Path>,
) -> Result<Option<(NodePlan, ContainerSpec)>, String> {
    let Some(plane) = &model.signal_plane else {
        return Ok(None);
    };
    let spec = vxedge_core::services::kuksa_spec(&plane.kuksa).map_err(|e| e.to_string())?;
    let gateway = GatewayPlan::from_model(model).map_err(|e| e.to_string())?;
    let nodes = vxedge_core::zones::plan_nodes_from_model(
        model,
        &gateway,
        dir.unwrap_or_else(|| Path::new(".")),
    )
    .map_err(|e| e.to_string())?;
    let on = &plane.kuksa.on;
    let node = nodes
        .into_iter()
        .find(|n| &n.node_id == on)
        .ok_or_else(|| format!("signal plane placed on `{on}`, which has no node plan"))?;
    Ok(Some((node, spec)))
}

/// Which stages a request asked for. Empty means all of them, which is what every caller that
/// predates staging sends — so the default stays "the whole experiment".
/// The lifecycle claim for a staged verb. The stages are part of it, so only an exact duplicate
/// gets the running job: `up --stage gateway` while a full `up` runs is refused as busy instead
/// of being handed a job that does more than it asked for.
pub(crate) fn staged_target(base: String, stages: &[i32]) -> String {
    if stages.is_empty() {
        return base;
    }
    let mut names: Vec<&str> = stages
        .iter()
        .map(|s| {
            ExperimentStage::try_from(*s)
                .map(|st| st.as_str_name())
                .unwrap_or("?")
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    format!("{base} [{}]", names.join(", "))
}

pub(crate) fn wants(stages: &[i32], stage: ExperimentStage) -> bool {
    stages.is_empty() || stages.contains(&(stage as i32))
}

/// Refuse a stage this daemon does not know. Reading it as "none of the stages" would run nothing
/// and report success.
pub(crate) fn check_stages(stages: &[i32]) -> Result<(), Status> {
    for stage in stages {
        match ExperimentStage::try_from(*stage) {
            Ok(ExperimentStage::Unspecified) | Err(_) => {
                return Err(crate::errors::invalid(
                    crate::errors::R::InvalidRequest,
                    format!("unknown stage {stage}"),
                ));
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

#[tonic::async_trait]
impl ExperimentService for ExperimentSvc {
    async fn up(&self, req: Request<ExperimentUpRequest>) -> Result<Response<Job>, Status> {
        check_stages(&req.get_ref().stages)?;
        // Host requirements first: refusing here costs nothing, and refusing halfway through
        // provisioning strands a partly-built topology on a host that was never going to run it.
        // Also the one resolution of the workspace directory: carried into every stage below
        // (`inline()`'s `resolved_dir`) so a local bundle/part ref resolves against the real
        // workspace instead of the daemon's own cwd once the document is re-wrapped as inline
        // YAML — found live 2026-08-04, `zone up` after `experiment up --stage gateway`.
        let dir = match req.get_ref().manifest.as_ref() {
            Some(mref) => crate::services::resolve_manifest_dir(mref).await,
            None => None,
        };
        let yaml = resolve_once(req.get_ref().manifest.as_ref()).await?;
        let model = vxedge_core::model::load_yaml(&yaml).map_err(|e| {
            crate::errors::invalid(crate::errors::R::WorkspaceInvalid, e.to_string())
        })?;
        // Every check `up` makes before touching the host, shared with `vehicle up` and
        // `WorkspaceService.Plan`.
        let nodes = GatewayPlan::from_model(&model)
            .map_err(|e| e.to_string())
            .and_then(|gw| {
                vxedge_core::zones::plan_nodes_from_model(
                    &model,
                    &gw,
                    dir.as_deref().unwrap_or_else(|| std::path::Path::new(".")),
                )
                .map_err(|e| e.to_string())
            })
            .map_err(|e| crate::errors::precondition(crate::errors::R::WorkspaceInvalid, e))?;
        let pf = crate::plan::preflight(
            "experiment up",
            &model.id,
            &model,
            &nodes,
            dir.as_deref(),
            &self.handles.zones.runtime_dir,
            &self.handles.gateway,
        )
        .await;
        if let Some(refusal) = pf.refusal() {
            return Err(refusal);
        }
        self.audit.record(
            "ExperimentService.Up",
            serde_json::json!({ "experiment": model.id }),
        );
        let id = model.id.clone();
        let svc = self.handles.clone();
        let stages = req.get_ref().stages.clone();
        let job = self
            .handles
            .engine
            .submit_with(
                JobOpts::lifecycle(&req, staged_target(format!("experiment {id}"), &stages)),
                "experiment.up",
                move |p| async move {
                    let mut done: Vec<String> = Vec::new();
                    p.checkpoint()?;
                    if wants(&stages, ExperimentStage::Gateway) {
                        svc.stage_gateway(&yaml, dir.as_deref(), true, &p).await?;
                        done.push("gateway".to_string());
                    }
                    p.checkpoint()?;
                    if wants(&stages, ExperimentStage::Zones) {
                        done.push(svc.stage_zones(true, &p).await?);
                    }
                    p.checkpoint()?;
                    if wants(&stages, ExperimentStage::Compute) {
                        done.push(svc.stage_compute(true, &p).await?);
                    }
                    p.checkpoint()?;
                    if wants(&stages, ExperimentStage::SignalPlane) {
                        done.push(
                            svc.stage_signal_plane(&model, dir.as_deref(), true, &p)
                                .await?,
                        );
                    }
                    p.checkpoint()?;
                    if wants(&stages, ExperimentStage::Services) {
                        done.push(
                            svc.stage_services(&model, &yaml, dir.as_deref(), true, &p)
                                .await?,
                        );
                    }
                    // Said explicitly, because an engineer expecting `up` to have started the load
                    // would otherwise sit waiting for numbers that are not coming.
                    Ok::<_, crate::jobs::JobFailure>(format!(
                        "experiment `{id}` up — {}. \
                     Traffic is not started by `up`: run `vxedge traffic up`.",
                        done.join(", ")
                    ))
                },
            )
            .await?;
        Ok(Response::new(job))
    }

    async fn down(&self, req: Request<ExperimentDownRequest>) -> Result<Response<Job>, Status> {
        check_stages(&req.get_ref().stages)?;
        // Same resolution `up` does (minus preflight, which only guards provisioning) — carried
        // into every stage below for the same reason: a local bundle/part ref, or the audit
        // sha256, should resolve against the real workspace, not the daemon's own cwd.
        let dir = match req.get_ref().manifest.as_ref() {
            Some(mref) => crate::services::resolve_manifest_dir(mref).await,
            None => None,
        };
        let yaml = resolve_once(req.get_ref().manifest.as_ref()).await?;
        let model = vxedge_core::model::load_yaml(&yaml).map_err(|e| {
            crate::errors::invalid(crate::errors::R::WorkspaceInvalid, e.to_string())
        })?;
        let active = self.handles.gateway.active_on_host().await;
        check_active_target(
            "experiment down",
            active
                .as_ref()
                .map(|(id, dir)| (id.as_str(), dir.as_path())),
            &model.id,
            dir.as_deref(),
        )?;
        self.audit.record(
            "ExperimentService.Down",
            serde_json::json!({ "experiment": model.id }),
        );
        let id = model.id.clone();
        let svc = self.handles.clone();
        let stages = req.get_ref().stages.clone();
        let job = self
            .handles
            .engine
            .submit_with(
                JobOpts::lifecycle(&req, staged_target(format!("experiment {id}"), &stages)),
                "experiment.down",
                move |p| async move {
                    // Reverse order, and **best-effort**: a teardown that stops at the first error
                    // strands whatever came after it. Collect failures, tear the rest down anyway,
                    // and report everything that went wrong.
                    let mut failures: Vec<JobFailure> = Vec::new();
                    if wants(&stages, ExperimentStage::Services)
                        && let Err(e) = svc
                            .stage_services(&model, &yaml, dir.as_deref(), false, &p)
                            .await
                    {
                        failures.push(e);
                    }
                    if wants(&stages, ExperimentStage::SignalPlane)
                        && let Err(e) = svc
                            .stage_signal_plane(&model, dir.as_deref(), false, &p)
                            .await
                    {
                        failures.push(e);
                    }
                    if wants(&stages, ExperimentStage::Compute)
                        && let Err(e) = svc.stage_compute(false, &p).await
                    {
                        failures.push(e);
                    }
                    if wants(&stages, ExperimentStage::Zones)
                        && let Err(e) = svc.stage_zones(false, &p).await
                    {
                        failures.push(e);
                    }
                    if wants(&stages, ExperimentStage::Gateway)
                        && let Err(e) = svc.stage_gateway(&yaml, dir.as_deref(), false, &p).await
                    {
                        failures.push(e);
                    }
                    if failures.is_empty() {
                        Ok::<_, JobFailure>(format!("experiment `{id}` down"))
                    } else {
                        Err(JobFailure::joined(
                            format!(
                                "experiment `{id}` down with {} failure(s); the rest was torn down",
                                failures.len()
                            ),
                            failures,
                        ))
                    }
                },
            )
            .await?;
        Ok(Response::new(job))
    }

    async fn status(
        &self,
        req: Request<ExperimentStatusRequest>,
    ) -> Result<Response<ExperimentStatus>, Status> {
        let dir = match req.get_ref().manifest.as_ref() {
            Some(mref) => crate::services::resolve_manifest_dir(mref).await,
            None => None,
        };
        let yaml = resolve_once(req.get_ref().manifest.as_ref()).await?;
        let model = vxedge_core::model::load_yaml(&yaml).map_err(|e| {
            crate::errors::invalid(crate::errors::R::WorkspaceInvalid, e.to_string())
        })?;
        // The components below describe what is active, so this must be what the caller named.
        let active = self.handles.gateway.active_on_host().await;
        check_active_target(
            "experiment status",
            active
                .as_ref()
                .map(|(id, dir)| (id.as_str(), dir.as_path())),
            &model.id,
            dir.as_deref(),
        )?;

        let gw = self
            .handles
            .gateway
            .status(Request::new(GatewayStatusRequest {}))
            .await?
            .into_inner();
        let gw_status = gw.status.clone().unwrap_or_default();

        let mut nodes = self
            .handles
            .zones
            .list(Request::new(vxedge_api::v1::ZoneListRequest::default()))
            .await
            .map(|r| r.into_inner().nodes)
            .unwrap_or_default();
        nodes.extend(
            self.handles
                .compute
                .list(Request::new(ComputeListRequest::default()))
                .await
                .map(|r| r.into_inner().node_status)
                .unwrap_or_default(),
        );
        let mut components: Vec<ResourceStatus> =
            nodes.iter().filter_map(|n| n.status.clone()).collect();
        for service_type in vxedge_core::services::ServiceDef::declared_types(&model) {
            let observed = self
                .handles
                .svcs
                .status(Request::new(vxedge_api::v1::SvcStatusRequest {
                    service_type,
                    experiment: inline(&yaml, dir.as_deref()),
                    ..Default::default()
                }))
                .await?;
            components.push(observed.into_inner());
        }
        components.extend(
            self.handles
                .signal_plane_component(&model, dir.as_deref())
                .await?,
        );

        let mut status = build_experiment_status(&model.id, gw_status, components);
        status.gateway = Some(gw);
        status.nodes = nodes;
        Ok(Response::new(status))
    }
}

/// Pure assembly of an `ExperimentStatus` from its already-fetched components — no I/O, so it
/// unit-tests directly rather than only through a live gRPC call needing real `Handles`.
/// Composite pattern (`vxedge_core::status::aggregate`): the experiment's own state is
/// *derived* from its components, never independently declared, so it can't silently disagree
/// with what's actually running underneath it. Also fixes a real edge case the old hand-rolled
/// `all_up` check got wrong: zero resources used to report `Degraded`
/// (`!resources.is_empty() && ...` is false either way an empty list gets there) — now
/// correctly `Down`, since nothing partially worked, nothing is declared at all. (The gateway
/// entry below is always present in the real handler, so that exact empty case can't occur
/// through `status()` today — kept general here since this function has no way to know that,
/// and testing the boundary directly is cheaper than trusting the caller's invariant.)
fn build_experiment_status(
    experiment_id: &str,
    gw_status: ResourceStatus,
    components: Vec<ResourceStatus>,
) -> ExperimentStatus {
    let (status, components) = crate::status::composite_rollup(
        experiment_id,
        ResourceKind::Experiment,
        gw_status,
        components,
    );
    ExperimentStatus {
        status: Some(status),
        components,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_claim_names_the_stages_so_only_an_exact_duplicate_matches() {
        let all = staged_target("vehicle car".into(), &[]);
        assert_eq!(all, "vehicle car");
        let gw = staged_target("vehicle car".into(), &[ExperimentStage::Gateway as i32]);
        assert_ne!(
            gw, all,
            "a gateway-only up is not a duplicate of a full one"
        );
        let a = staged_target(
            "vehicle car".into(),
            &[
                ExperimentStage::Zones as i32,
                ExperimentStage::Gateway as i32,
            ],
        );
        let b = staged_target(
            "vehicle car".into(),
            &[
                ExperimentStage::Gateway as i32,
                ExperimentStage::Zones as i32,
                ExperimentStage::Gateway as i32,
            ],
        );
        assert_eq!(a, b, "order and repeats do not matter");
    }

    fn status(
        vxid: &str,
        kind: ResourceKind,
        state: vxedge_api::v1::ResourceState,
    ) -> ResourceStatus {
        ResourceStatus {
            vxid: vxid.into(),
            kind: kind as i32,
            state: state as i32,
            detail: String::new(),
            since_unix_ms: 0,
            ..Default::default()
        }
    }

    #[test]
    fn a_healthy_gateway_and_no_zones_is_up() {
        let gw = status(
            "vxgateway",
            ResourceKind::Gateway,
            vxedge_api::v1::ResourceState::Up,
        );
        let out = build_experiment_status("exp-a", gw, vec![]);
        assert_eq!(
            out.status.unwrap().state,
            vxedge_api::v1::ResourceState::Up as i32
        );
    }

    #[test]
    fn a_down_zone_degrades_an_otherwise_healthy_experiment() {
        let gw = status(
            "vxgateway",
            ResourceKind::Gateway,
            vxedge_api::v1::ResourceState::Up,
        );
        let zone = status(
            "node-a",
            ResourceKind::Zone,
            vxedge_api::v1::ResourceState::Down,
        );
        let out = build_experiment_status("exp-a", gw, vec![zone]);
        assert_eq!(
            out.status.unwrap().state,
            vxedge_api::v1::ResourceState::Degraded as i32
        );
    }

    #[test]
    fn a_down_gateway_and_no_zones_is_down_not_degraded() {
        // The exact bug this rollup used to have: zero-or-uniformly-down must report Down,
        // never Degraded, since nothing partially worked.
        let gw = status(
            "vxgateway",
            ResourceKind::Gateway,
            vxedge_api::v1::ResourceState::Down,
        );
        let out = build_experiment_status("exp-a", gw, vec![]);
        assert_eq!(
            out.status.unwrap().state,
            vxedge_api::v1::ResourceState::Down as i32
        );
    }

    #[test]
    fn the_gateway_entry_is_always_first_and_correctly_kinded() {
        let gw = status(
            "vxgateway",
            ResourceKind::Gateway,
            vxedge_api::v1::ResourceState::Up,
        );
        let zone = status(
            "node-a",
            ResourceKind::Zone,
            vxedge_api::v1::ResourceState::Up,
        );
        let out = build_experiment_status("exp-a", gw, vec![zone]);
        assert_eq!(out.components.len(), 2);
        assert_eq!(out.components[0].vxid, "vxgateway");
        assert_eq!(out.components[0].kind, ResourceKind::Gateway as i32);
        assert_eq!(out.components[1].vxid, "node-a");
        assert_eq!(out.components[1].kind, ResourceKind::Zone as i32);
    }

    #[test]
    fn the_experiment_status_carries_the_supplied_id_and_kind() {
        let gw = status(
            "vxgateway",
            ResourceKind::Gateway,
            vxedge_api::v1::ResourceState::Up,
        );
        let out = build_experiment_status("exp-a", gw, vec![]);
        let s = out.status.unwrap();
        assert_eq!(s.vxid, "exp-a");
        assert_eq!(s.kind, ResourceKind::Experiment as i32);
    }

    // `ExperimentService.Down` used to stop every zone and the gateway of whichever experiment
    // was active, whatever workspace it was asked to bring down. These pin the guard that
    // refuses a Down aimed at anything but the active experiment.
    use std::path::Path;

    #[test]
    fn down_with_nothing_active_proceeds() {
        assert!(
            check_active_target("experiment down", None, "exp-a", Some(Path::new("/ws/a"))).is_ok()
        );
    }

    #[test]
    fn down_of_the_active_experiment_proceeds() {
        let active = Some(("exp-a", Path::new("/ws/a")));
        assert!(
            check_active_target("experiment down", active, "exp-a", Some(Path::new("/ws/a")))
                .is_ok()
        );
    }

    #[test]
    fn down_of_a_different_experiment_is_refused_naming_the_active_one() {
        let active = Some(("exp-a", Path::new("/ws/a")));
        let err = check_active_target("experiment down", active, "exp-b", Some(Path::new("/ws/b")))
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("`exp-a`"), "{}", err.message());
        assert!(err.message().contains("/ws/a"), "{}", err.message());
        assert!(err.message().contains("`exp-b`"), "{}", err.message());
    }

    #[test]
    fn down_of_the_same_id_from_another_workspace_is_refused() {
        // Two copies of one experiment in different directories: only the one that was
        // brought up is torn down from its own workspace.
        let active = Some(("exp-a", Path::new("/ws/a")));
        let err = check_active_target(
            "experiment down",
            active,
            "exp-a",
            Some(Path::new("/ws/copy-of-a")),
        )
        .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("/ws/a"), "{}", err.message());
    }

    #[test]
    fn an_inline_down_with_no_directory_is_matched_on_id_alone() {
        let active = Some(("exp-a", Path::new("/ws/a")));
        assert!(check_active_target("experiment down", active, "exp-a", None).is_ok());
        assert!(check_active_target("experiment down", active, "exp-b", None).is_err());
    }

    #[test]
    fn an_active_experiment_with_no_known_directory_is_matched_on_id_alone() {
        // `gateway up` from a genuinely inline manifest records "." as the directory.
        let active = Some(("exp-a", Path::new(".")));
        assert!(
            check_active_target("experiment down", active, "exp-a", Some(Path::new("/ws/a")))
                .is_ok()
        );
    }

    /// The parent has to show a sub-job's step while that step is running, not after it.
    #[tokio::test]
    async fn await_job_relays_sub_job_progress_to_the_parent() {
        let engine = JobEngine::new(crate::events::EventBus::new());
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let sub = engine
            .submit("sub", move |p| async move {
                p.update(50, "moving port");
                let _ = gate.await;
                Ok::<_, String>("done".to_string())
            })
            .await;
        let inner = engine.clone();
        let parent = engine
            .submit("parent", move |p| async move {
                await_job(&inner, sub, &p, 30, "zone a").await
            })
            .await;
        let mut rx = engine.watch(&parent.id).await.unwrap();
        while rx.borrow_and_update().message != "zone a: moving port" {
            rx.changed().await.unwrap();
        }
        release.send(()).unwrap();
        while !is_terminal(rx.borrow_and_update().state) {
            rx.changed().await.unwrap();
        }
        assert_eq!(rx.borrow().state, JobState::Succeeded as i32);
    }

    #[test]
    fn a_down_refusal_names_the_verb_it_refused() {
        let active = Some(("vehicle-a", Path::new("/ws/a")));
        for verb in ["vehicle up", "vehicle status", "vehicle down"] {
            let err = check_active_target(verb, active, "vehicle-b", None).unwrap_err();
            assert!(err.message().contains(&format!("`{verb}`")), "{err:?}");
        }
    }
}
