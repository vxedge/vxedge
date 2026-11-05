//! `WorkspaceService` and `HostService`: what `up` would do, and what this host can run.
//!
//! [`preflight`] is the one list of checks `up` runs before touching the host. `vehicle up` and
//! `experiment up` refuse on its first problem, and `Plan` reports all of them, so the two cannot
//! disagree about whether a workspace would come up.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tonic::{Request, Response, Status};

use vxedge_api::v1::host_service_server::HostService;
use vxedge_api::v1::workspace_service_server::WorkspaceService;
use vxedge_api::v1::{
    BackendCapability, HostCapabilities, HostCapabilitiesRequest, HostCheck, HostGpu, PlanProblem,
    PlannedService, WorkspacePlan, WorkspacePlanRequest,
};
use vxedge_core::containers::LazyClient;
use vxedge_core::gateway::GatewayPlan;
use vxedge_core::model::{Backend, Experiment};
use vxedge_core::package::BackendProbe;
use vxedge_core::zones::NodePlan;

use crate::errors::{self, R};
use crate::services::GatewaySvc;

/// What `up` found before touching the host.
#[derive(Default)]
pub(crate) struct Preflight {
    pub problems: Vec<PlanProblem>,
    pub checks: Vec<HostCheck>,
    /// The backends probed for `requires.backends`, kept so `Plan` asks Podman once.
    pub backends: Option<Vec<BackendProbe>>,
}

impl Preflight {
    fn check(&mut self, name: &str, failure: Option<(R, String, String)>) {
        let passed = failure.is_none();
        let detail = match failure {
            Some((reason, subject, detail)) => {
                self.problems.push(PlanProblem {
                    reason: reason as i32,
                    subject,
                    detail: detail.clone(),
                });
                detail
            }
            None => String::new(),
        };
        self.checks.push(HostCheck {
            name: name.to_string(),
            passed,
            detail,
        });
    }

    /// The refusal `up` gives: its first problem.
    pub fn refusal(&self) -> Option<Status> {
        let p = self.problems.first()?;
        let reason = R::try_from(p.reason).unwrap_or(R::Unspecified);
        Some(match reason {
            R::BackendUnavailable => errors::unavailable(reason, p.detail.clone()),
            _ => errors::precondition(reason, p.detail.clone()),
        })
    }
}

fn lines(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| format!("  {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every check `up` makes before it touches the host, in the order it makes them.
pub(crate) async fn preflight(
    verb: &str,
    who: &str,
    model: &Experiment,
    nodes: &[NodePlan],
    dir: Option<&Path>,
    runtime_dir: &Path,
    gateway: &GatewaySvc,
) -> Preflight {
    let mut pf = Preflight::default();
    let workspace = dir.unwrap_or_else(|| Path::new("."));

    // The daemon reads a directory as joined documents, which carry no `.vxp`, so the
    // descriptor's agreement with them is checked here, from the directory itself, with the
    // same function `experiment validate` uses. Document errors are not repeated: the model
    // build already reported them.
    let package = dir.and_then(|d| match vxedge_core::Workspace::load(d) {
        Ok(ws) => {
            let problems = ws.package_problems();
            (!problems.is_empty()).then(|| {
                let detail = problems
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ");
                (R::WorkspaceInvalid, detail)
            })
        }
        // A newer schema is its own reason, so an agent can tell "upgrade vxedge" from "fix the
        // descriptor" without reading the sentence.
        Err(vxedge_core::WorkspaceError::Package(
            e @ vxedge_core::DiscoverError::UnsupportedSchema { .. },
        )) => Some((R::PackageSchemaUnsupported, e.to_string())),
        Err(vxedge_core::WorkspaceError::Package(e)) => Some((R::WorkspaceInvalid, e.to_string())),
        Err(_) => None,
    });
    pf.check(
        "package descriptor",
        package.map(|(reason, detail)| (reason, model.id.clone(), detail)),
    );

    let version = crate::services::preflight_vxedge_version(dir).err();
    pf.check(
        "vxedge version",
        version.map(|s| {
            (
                R::VxedgeVersionUnsupported,
                model.id.clone(),
                s.message().to_string(),
            )
        }),
    );

    let (unmet, probed) = host_requirements(dir).await;
    pf.backends = probed;
    pf.check(
        "host requirements",
        unmet.map(|detail| (R::HostRequirementUnmet, model.id.clone(), detail)),
    );

    let active = gateway.active_on_host().await;
    let target = crate::experiment::check_active_target(
        verb,
        active
            .as_ref()
            .map(|(id, dir)| (id.as_str(), dir.as_path())),
        &model.id,
        dir,
    )
    .err();
    pf.check(
        "active workspace",
        target.map(|s| (R::WrongWorkspace, model.id.clone(), s.message().to_string())),
    );

    let plans = nodes.to_vec();
    let foreign = tokio::task::spawn_blocking(move || vxedge_core::zones::foreign_objects(&plans))
        .await
        .unwrap_or_default();
    pf.check(
        "other vehicles",
        (!foreign.is_empty()).then(|| {
            (
                R::ForeignWorkload,
                model.id.clone(),
                format!(
                    "this host is running what `{}` does not declare: {}. Another vehicle is up, \
                     possibly under a daemon with a different runtime dir, and `{verb}` would act on \
                     its gateway. Run it from that workspace, or bring that vehicle down first",
                    who,
                    foreign.join(", ")
                ),
            )
        }),
    );

    let placed = model
        .services
        .values()
        .filter(|s| s.on.is_some())
        .find_map(|def| vxedge_core::services::placed_spec(def, workspace).err());
    pf.check(
        "placed services",
        placed.map(|e| {
            let text = e.to_string();
            let reason = if text.contains("artifact store") {
                R::ArtifactStoreMissing
            } else {
                R::WorkspaceInvalid
            };
            (reason, model.id.clone(), text)
        }),
    );

    let images = local_images(model, nodes);
    let image_problem = if images.is_empty() {
        None
    } else {
        match LazyClient::user().get().await {
            Err(e) => Some((R::BackendUnavailable, "podman".to_string(), e.to_string())),
            Ok(client) => {
                let mut missing = Vec::new();
                let mut failure = None;
                for image in &images {
                    match client.image_present(image).await {
                        Ok(true) => {}
                        Ok(false) => missing.push(format!("  {image}")),
                        Err(e) => {
                            failure = Some((R::BackendUnavailable, image.clone(), e.to_string()));
                            break;
                        }
                    }
                }
                failure.or_else(|| {
                    (!missing.is_empty()).then(|| {
                        (
                            R::LocalImageMissing,
                            model.id.clone(),
                            format!(
                                "`{}` runs local images that are not in this host's Podman store:\n{}\n\
                                 A `localhost/` image is built per host. Build it with the \
                                 workspace's build script, then run `{verb}` again.",
                                who,
                                missing.join("\n")
                            ),
                        )
                    })
                })
            }
        }
    };
    pf.check("local images", image_problem);

    let bundles = dir
        .map(|d| vxedge_core::zones::missing_local_bundles(model, d))
        .unwrap_or_default();
    pf.check(
        "guest bundles",
        (!bundles.is_empty()).then(|| {
            (
                R::LocalBundleMissing,
                model.id.clone(),
                format!(
                    "`{}` boots a local guest bundle that has not been built on this host:\n{}\n\
                     A bundle embeds the host kernel, so it is built per host and never published. \
                     Build it with `<vxparts>/parts/zone/linux/stack/probe-bundle/build.sh <dir \
                     above>` (needs busybox-static and a readable /boot/vmlinuz-$(uname -r); see \
                     that part's README), then run `{verb}` again.",
                    who,
                    lines(&bundles)
                ),
            )
        }),
    );

    let modules = dir
        .map(|d| vxedge_core::zones::missing_local_modules(model, d))
        .unwrap_or_default();
    pf.check(
        "capsule modules",
        (!modules.is_empty()).then(|| {
            (
                R::CapsuleModuleMissing,
                model.id.clone(),
                format!(
                    "`{}` runs a capsule module that is not on this host:\n{}\nBuild the module, \
                     or correct the part's `ref`, then run `{verb}` again.",
                    who,
                    lines(&modules)
                ),
            )
        }),
    );

    let socket = nodes
        .iter()
        .filter(|n| n.backend == Backend::CloudHypervisor)
        .find_map(|n| {
            vxedge_core::zones::ch_socket_too_long(n, runtime_dir).map(|e| (n.node_id.clone(), e))
        });
    pf.check(
        "socket paths",
        socket.map(|(node, e)| (R::RuntimeDirTooLong, node, e)),
    );
    pf
}

/// Every `localhost/` image the workspace runs: payloads and everything placed on a device.
pub(crate) fn local_images(model: &Experiment, nodes: &[NodePlan]) -> BTreeSet<String> {
    all_images(model, nodes)
        .into_iter()
        .filter(|i| i.starts_with("localhost/"))
        .collect()
}

fn all_images(model: &Experiment, nodes: &[NodePlan]) -> BTreeSet<String> {
    let payloads = nodes.iter().filter_map(|n| n.image.clone());
    let placed = model.placements().map(|p| p.image.url.clone());
    payloads.chain(placed).collect()
}

/// What a package's `requires` asks of this host and does not get, **before** anything is
/// provisioned: every unmet requirement, joined, and the backend probes taken on the way.
///
/// Daemon-side on purpose: the host that matters is the one doing the provisioning, and a
/// CLI-side check asks the wrong machine the moment `--endpoint` points elsewhere. An inline
/// manifest, or a bare directory of documents, has no descriptor, so nothing is checked and
/// nothing is claimed.
async fn host_requirements(dir: Option<&Path>) -> (Option<String>, Option<Vec<BackendProbe>>) {
    let Some(found) = dir.and_then(|d| vxedge_core::discover(d).ok()) else {
        return (None, None);
    };
    let requires = found.package.host_requires();
    // Podman is asked only when the package names backends.
    let probed = if requires.backends.is_empty() {
        None
    } else {
        Some(backend_probes().await)
    };
    let host = vxedge_core::package::HostCapabilities {
        backends: probed.clone().unwrap_or_default(),
        ..tokio::task::spawn_blocking(vxedge_core::package::HostCapabilities::probe)
            .await
            .unwrap_or_default()
    };
    let unmet = requires
        .check(found.package.id(), &host)
        .err()
        .map(|unmet| {
            unmet
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" ")
        });
    (unmet, probed)
}

pub struct PlanSvc {
    pub gateway: Arc<GatewaySvc>,
    pub runtime_dir: PathBuf,
}

fn on_path(binary: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(binary).is_file()))
}

/// What this host can run, from the checks `up` itself makes: Podman for container nodes (the
/// preflight's image check), the emulator the qemu-microvm backend starts, and the GPU probe a
/// workspace's `requires` is checked against.
async fn backends() -> (Vec<BackendCapability>, Option<HostGpu>) {
    let probes = backend_probes().await;
    let gpu = tokio::task::spawn_blocking(|| vxedge_core::package::HostCapabilities::probe().gpu)
        .await
        .unwrap_or_default()
        .map(|g| HostGpu {
            compute_capability: g.compute_cap,
            vram_mib: g.vram_mib,
        });
    (probes.iter().map(capability).collect(), gpu)
}

fn capability(p: &BackendProbe) -> BackendCapability {
    BackendCapability {
        backend: crate::status::node_backend(p.backend) as i32,
        available: p.unavailable.is_none(),
        detail: p.unavailable.clone().unwrap_or_default(),
    }
}

/// Whether each backend can run here, and why not when it cannot.
async fn backend_probes() -> Vec<BackendProbe> {
    let podman = LazyClient::user()
        .get()
        .await
        .map(|_| ())
        .map_err(|e| e.to_string());
    tokio::task::spawn_blocking(move || probe_backends(podman))
        .await
        .unwrap_or_default()
}

fn probe_backends(podman: Result<(), String>) -> Vec<BackendProbe> {
    let kvm = Path::new("/dev/kvm").exists();
    let entry = |backend: Backend, available: bool, detail: String| BackendProbe {
        backend,
        unavailable: (!available).then_some(detail),
    };
    let qemu = vxedge_core::zones::QEMU_MICROVM_BIN;
    let container = match (&podman, on_path("ip")) {
        (Err(e), _) => format!("Podman did not answer: {e}"),
        (Ok(()), _) => "`ip` (iproute2) is not on PATH".to_string(),
    };
    vec![
        entry(
            Backend::Container,
            podman.is_ok() && on_path("ip"),
            container,
        ),
        entry(
            Backend::QemuMicrovm,
            kvm && on_path(qemu),
            format!("needs /dev/kvm and `{qemu}` on PATH"),
        ),
        entry(
            Backend::Libvirt,
            on_path("virsh"),
            "`virsh` is not on PATH".to_string(),
        ),
        entry(
            Backend::CloudHypervisor,
            kvm && vxedge_core::zones::ch_available(),
            "needs /dev/kvm and cloud-hypervisor".to_string(),
        ),
        entry(
            Backend::WasmCapsule,
            vxedge_core::zones::capsule_runner_available(),
            "no `vxcapsule` runner beside vxedged, on PATH or in VXEDGE_CAPSULE_BIN".to_string(),
        ),
    ]
}

#[tonic::async_trait]
impl WorkspaceService for PlanSvc {
    async fn plan(
        &self,
        req: Request<WorkspacePlanRequest>,
    ) -> Result<Response<WorkspacePlan>, Status> {
        let (yaml, _, dir) =
            crate::services::resolve_manifest(req.get_ref().manifest.as_ref()).await?;
        let invalid = |detail: String| WorkspacePlan {
            valid: false,
            problems: vec![PlanProblem {
                reason: R::WorkspaceInvalid as i32,
                subject: "workspace".to_string(),
                detail,
            }],
            ..Default::default()
        };
        let model = match vxedge_core::model::load_workspace_yaml(&yaml) {
            Ok(m) => m,
            Err(e) => return Ok(Response::new(invalid(e.to_string()))),
        };
        let planned = GatewayPlan::from_model(&model)
            .map_err(|e| e.to_string())
            .and_then(|gw| {
                vxedge_core::zones::plan_nodes_from_model(
                    &model,
                    &gw,
                    dir.as_deref().unwrap_or_else(|| Path::new(".")),
                )
                .map_err(|e| e.to_string())
            });
        let nodes = match planned {
            Ok(n) => n,
            Err(e) => return Ok(Response::new(invalid(e))),
        };
        let who = model
            .entities
            .first()
            .map(|e| e.id.clone())
            .unwrap_or_else(|| model.id.clone());
        let pf = preflight(
            "up",
            &who,
            &model,
            &nodes,
            dir.as_deref(),
            &self.runtime_dir,
            &self.gateway,
        )
        .await;
        let probes = match pf.backends {
            Some(p) => p,
            None => backend_probes().await,
        };
        let mut host_checks = pf.checks;
        for p in probes
            .into_iter()
            .filter(|p| nodes.iter().any(|n| n.backend == p.backend))
        {
            host_checks.push(HostCheck {
                name: format!("backend {}", p.backend.as_str()),
                passed: p.unavailable.is_none(),
                detail: p.unavailable.unwrap_or_default(),
            });
        }
        let mut services: Vec<PlannedService> = model
            .services
            .values()
            .map(|s| PlannedService {
                vxid: s.id.clone(),
                service_type: s.service_type.as_str().to_string(),
                on: s.on.clone().unwrap_or_default(),
                image: s.image.url.clone(),
            })
            .collect();
        if let Some(plane) = &model.signal_plane {
            services.push(PlannedService {
                vxid: vxedge_core::services::KUKSA_MEMBER.to_string(),
                service_type: "signal-plane".to_string(),
                on: plane.kuksa.on.clone(),
                image: plane.kuksa.image.url.clone(),
            });
        }
        Ok(Response::new(WorkspacePlan {
            valid: pf.problems.is_empty(),
            problems: pf.problems,
            nodes: nodes.iter().map(crate::status::node_spec).collect(),
            services,
            images: all_images(&model, &nodes).into_iter().collect(),
            host_checks,
        }))
    }
}

#[tonic::async_trait]
impl HostService for PlanSvc {
    async fn capabilities(
        &self,
        _: Request<HostCapabilitiesRequest>,
    ) -> Result<Response<HostCapabilities>, Status> {
        let (backends, gpu) = backends().await;
        Ok(Response::new(HostCapabilities {
            backends,
            part_kinds: vec!["container".into(), "bundle".into(), "wasm".into()],
            max_runs: 1,
            gpu,
            package_schemas: vxedge_core::package::KNOWN_SCHEMAS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }))
    }
}
