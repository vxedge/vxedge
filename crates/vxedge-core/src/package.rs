//! The `vxp.yaml` package descriptor, and discovering a workspace from a directory.
//!
//! A workspace is addressed the way Terraform addresses a configuration: **the working
//! directory is the unit of work**. `vxedge experiment up` takes no name — it looks for the
//! `vxp.yaml` in the directory it was run from.
//!
//! Two properties are deliberate, and both are Terraform's:
//!
//! 1. **No walking up to parent directories.** `git` searches upward; Terraform does not, and
//!    neither do we. Running from inside `results/` must fail rather than silently resolve the
//!    parent experiment and re-provision it — the same reasoning that made
//!    [`crate::Workspace::load`] non-recursive.
//! 2. **Exactly one.** A directory without `vxp.yaml` is not a workspace, and one that also
//!    holds a `*.vxp` is refused, naming what it found, because the fix depends on what it is.
//!
//! The descriptor is separate from `main.yaml` so a version bump never touches experiment
//! content — the identity of a package and the content of an experiment change for different
//! reasons and at different times.
//!
//! **One descriptor form** (`ARCHITECTURE.md` §6.0, §6.0.3): `vxp.yaml` with `schema: vxp/1`,
//! beside the `vxp.lock` that `vxedge pkg lock` writes. `.vxp` names the packed file, so a
//! `*.vxp` holding a tar archive is refused here rather than parsed. A `<name>.vxp` YAML file is
//! the legacy descriptor of vxedge 0.5.x, which is no longer read: it is refused with the steps
//! to migrate it ([`DiscoverError::LegacyDescriptor`]).
//!
//! The `schema` is read from the raw mapping before anything else, so a newer schema is a
//! typed refusal ([`DiscoverError::UnsupportedSchema`]) and never a parse error.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::model::Backend;

/// The fixed descriptor name (`ARCHITECTURE.md` §6.0, decision X-1).
pub const DESCRIPTOR_FILE: &str = "vxp.yaml";
/// The lock `vxedge pkg lock` writes beside the descriptor (§6.0.7).
pub const LOCK_FILE: &str = "vxp.lock";
/// The package format this build reads and writes.
pub const SCHEMA_V1: &str = "vxp/1";
/// Every `schema` this build reads, as `HostService.Capabilities` reports them.
pub const KNOWN_SCHEMAS: &[&str] = &[SCHEMA_V1];

/// A `vxp/1` package descriptor (`ARCHITECTURE.md` §6.0.3): what this workspace *is*, as
/// distinct from what it *does*.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageDoc {
    pub id: PackageId,
    pub version: semver::Version,
    pub entry: String,
    pub title: Option<String>,
    pub license: Option<String>,
    pub authors: Vec<Author>,
    pub requires: RequiresV1,
    pub provenance: Option<Provenance>,
    /// `x-` keys, carried and ignored.
    pub extensions: BTreeMap<String, serde_yaml_ng::Value>,
}

impl PackageDoc {
    /// The id the `kind: experiment` document must carry: the `name` half of `namespace/name`.
    pub fn experiment_id(&self) -> &str {
        self.id.name()
    }

    /// The vxedge versions this package runs on, when it says.
    pub fn vxedge_range(&self) -> Option<&semver::VersionReq> {
        self.requires.vxedge.as_ref()
    }

    /// What [`Requires::check`] holds the host to.
    pub fn host_requires(&self) -> Requires {
        Requires {
            gpu: self.requires.gpu.map(|g| g.as_str().to_string()),
            kvm: self.requires.kvm.unwrap_or(false),
            preempt_rt: self.requires.kernel.as_ref().and_then(|k| k.preempt_rt),
            backends: self.requires.backends.clone(),
            simulators: self.requires.simulator.clone(),
        }
    }
}

/// The `vxp/1` fields, without `schema` and the `x-` keys, which are split off first.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V1Fields {
    id: PackageId,
    version: semver::Version,
    entry: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    authors: Vec<Author>,
    #[serde(default)]
    requires: RequiresV1,
    #[serde(default)]
    provenance: Option<Provenance>,
}

/// `namespace/name`: the namespace is the publishing organisation, the name the experiment id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PackageId {
    full: String,
    slash: usize,
}

impl PackageId {
    pub fn as_str(&self) -> &str {
        &self.full
    }
    pub fn namespace(&self) -> &str {
        &self.full[..self.slash]
    }
    pub fn name(&self) -> &str {
        &self.full[self.slash + 1..]
    }
}

impl TryFrom<String> for PackageId {
    type Error = String;
    fn try_from(full: String) -> Result<Self, String> {
        let component = |c: &str| {
            c.bytes()
                .next()
                .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                && c.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        };
        match full.split_once('/') {
            Some((ns, name)) if component(ns) && component(name) => Ok(Self {
                slash: ns.len(),
                full,
            }),
            _ => Err(format!(
                "package id `{full}` must be `namespace/name`, each lower-case letters, digits \
                 and `-`, starting with a letter or digit (e.g. `vxedge/h1-slice-isolation`)"
            )),
        }
    }
}

impl From<PackageId> for String {
    fn from(id: PackageId) -> String {
        id.full
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Author {
    pub org: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub generated_by: GeneratedBy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedBy {
    pub tool: String,
    pub version: String,
    #[serde(default)]
    pub model_digest: Option<String>,
}

/// `requires` under `vxp/1`. Every value is checked for shape when read, and the daemon's
/// preflight holds the host to it through [`PackageDoc::host_requires`].
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequiresV1 {
    #[serde(default)]
    pub vxedge: Option<semver::VersionReq>,
    #[serde(default)]
    pub gpu: Option<GpuTier>,
    #[serde(default)]
    pub kvm: Option<bool>,
    #[serde(default)]
    pub kernel: Option<KernelRequires>,
    #[serde(default, deserialize_with = "backends")]
    pub backends: Vec<Backend>,
    /// The simulators the package's scenario needs (`esmini`, `carla`), each on the host.
    #[serde(default, deserialize_with = "simulators")]
    pub simulator: Vec<String>,
}

fn simulators<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let names = Vec::<String>::deserialize(d)?;
    for n in &names {
        if !crate::scenario::SIMULATORS.contains(&n.as_str()) {
            return Err(serde::de::Error::custom(format!(
                "unknown simulator `{n}`; expected one of {}",
                crate::scenario::SIMULATORS.join(", ")
            )));
        }
    }
    Ok(names)
}

fn backends<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Backend>, D::Error> {
    Vec::<String>::deserialize(d)?
        .iter()
        .map(|s| {
            s.parse().map_err(|_| {
                serde::de::Error::custom(format!(
                    "unknown backend `{s}`; expected one of {}",
                    Backend::ALL.map(Backend::as_str).join(", ")
                ))
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuTier {
    None,
    Tier2,
}

impl GpuTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Tier2 => "tier2",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelRequires {
    #[serde(default)]
    pub preempt_rt: Option<PreemptRt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreemptRt {
    Required,
    Preferred,
    Any,
}

/// What a workspace needs from its host, checked by [`Requires::check`] before `up` provisions
/// anything.
///
/// This is what lets a GPU-dependent experiment be authored, validated and committed while
/// remaining inert: `exp-011` can exist as a sandbox before any Tier 2 host does, and the
/// unresolved GPU decision blocks nothing else. The check runs **daemon-side**, against the
/// host that would do the provisioning — a CLI-side check would be asking the wrong machine as
/// soon as `--endpoint` points somewhere else.
#[derive(Debug, Clone, Default)]
pub struct Requires {
    /// `none` | `tier2`, per `TESTBED-SETUP.md`'s host tiers.
    pub gpu: Option<String>,
    /// `/dev/kvm` must exist.
    pub kvm: bool,
    /// `required` refuses a kernel without PREEMPT_RT; `preferred` and `any` run on any kernel.
    pub preempt_rt: Option<PreemptRt>,
    /// Every backend listed must be available on this host.
    pub backends: Vec<Backend>,
    /// Every simulator listed must be on this host ([`crate::scenario::available_simulators`]).
    pub simulators: Vec<String>,
}

/// Tier 2's GPU floor, from `TESTBED-SETUP.md`: RTX-class, >= 12 GB VRAM.
///
/// **Compute capability is the harder half and the one to lead with.** DeepStream/TensorRT
/// FP16/INT8 need tensor cores, so a pre-Turing card is excluded on *architecture* rather than
/// memory — adding VRAM within such a generation does not help. 7.0 is Volta/Turing, the first
/// with tensor cores.
const TIER2_MIN_COMPUTE_CAP: f32 = 7.0;
const TIER2_MIN_VRAM_MIB: u64 = 12_000;

/// What the host offers, as far as a workspace's `requires` cares.
///
/// Separate from the check so the decision is pure and testable: probing is one function, and
/// "does this host satisfy this workspace" is another that takes the answer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostCapabilities {
    /// The best NVIDIA GPU the driver reports, as `(compute capability, VRAM MiB)`.
    ///
    /// Both, not merely "a GPU is present": an entry-level card such as a GT 1030 is a real
    /// NVIDIA GPU, but a *Pascal* one with 2 GB, so it fails Tier 2 on both counts. A presence check would have let `exp-011` start there and fail deep inside
    /// CARLA, which is exactly what the preflight exists to prevent.
    pub gpu: Option<GpuCapability>,
    /// `/dev/kvm` exists.
    pub kvm: bool,
    /// The running kernel is a PREEMPT_RT one ([`kernel_is_preempt_rt`]).
    pub preempt_rt: bool,
    /// The backends the daemon probed. [`HostCapabilities::probe`] leaves this empty: whether
    /// Podman answers is an async question the daemon asks, so it fills this in. A backend not
    /// listed counts as unavailable, never as present.
    pub backends: Vec<BackendProbe>,
    /// The simulators on vxedged's `PATH` ([`crate::scenario::available_simulators`]).
    pub simulators: Vec<String>,
}

/// One backend as the daemon found it: `unavailable` says why it cannot run here.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendProbe {
    pub backend: Backend,
    pub unavailable: Option<String>,
}

/// Whether the running kernel is PREEMPT_RT, from what it exposes. `/sys/kernel/realtime` decides
/// when present; a kernel without it says so only in its version string (`uname -v`). Pure.
pub fn kernel_is_preempt_rt(realtime: Option<&str>, version: &str) -> bool {
    match realtime {
        Some(flag) => flag.trim() == "1",
        None => version.contains("PREEMPT_RT"),
    }
}

/// Whether the running kernel can run `taprio`, the qdisc that renders `network.shaping`'s gate
/// schedule: its module is loaded, built in, or installed for this kernel, where `tc` loads it on
/// first use. Read from files any user may read, so an unprivileged daemon answers it too.
pub fn kernel_has_taprio() -> bool {
    let release = RunningKernel::probe().release;
    let modules = Path::new("/lib/modules").join(&release);
    let read = |name: &str| std::fs::read_to_string(modules.join(name)).ok();
    taprio_available(
        Path::new("/sys/module/sch_taprio").exists(),
        read("modules.builtin").as_deref(),
        read("modules.dep").as_deref(),
    )
}

/// [`kernel_has_taprio`]'s decision, from what the kernel exposes: `loaded` is
/// `/sys/module/sch_taprio`, and the two lists are the kernel's `modules.builtin` and
/// `modules.dep`, whose lines name a module by its path (`kernel/net/sched/sch_taprio.ko.zst:`).
/// Pure.
pub fn taprio_available(loaded: bool, builtin: Option<&str>, dep: Option<&str>) -> bool {
    let lists = |list: Option<&str>| {
        list.unwrap_or_default().lines().any(|l| {
            let path = l.split(':').next().unwrap_or_default().trim();
            let file = path.rsplit('/').next().unwrap_or_default();
            file == "sch_taprio.ko" || file.starts_with("sch_taprio.ko.")
        })
    };
    loaded || lists(builtin) || lists(dep)
}

/// The kernel this process runs on, as every run records it (`ARCHITECTURE.md` §13):
/// `kernel.preempt_rt: preferred` runs on either kind, so the result has to say which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningKernel {
    /// `/proc/sys/kernel/osrelease`, as `uname -r` prints it. Empty when it cannot be read.
    pub release: String,
    /// See [`kernel_is_preempt_rt`].
    pub preempt_rt: bool,
}

impl RunningKernel {
    pub fn probe() -> Self {
        let read = |path: &str| std::fs::read_to_string(path).ok();
        let version = read("/proc/sys/kernel/version").unwrap_or_default();
        Self {
            release: read("/proc/sys/kernel/osrelease")
                .map(|r| r.trim().to_string())
                .unwrap_or_default(),
            preempt_rt: kernel_is_preempt_rt(read("/sys/kernel/realtime").as_deref(), &version),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpuCapability {
    pub compute_cap: f32,
    pub vram_mib: u64,
}

impl GpuCapability {
    fn meets_tier2(&self) -> bool {
        self.compute_cap >= TIER2_MIN_COMPUTE_CAP && self.vram_mib >= TIER2_MIN_VRAM_MIB
    }

    fn describe(&self) -> String {
        format!(
            "compute capability {:.1}, {} MiB VRAM",
            self.compute_cap, self.vram_mib
        )
    }
}

impl HostCapabilities {
    /// Probe the host this process runs on: everything but [`Self::backends`].
    pub fn probe() -> Self {
        let out = std::process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=compute_cap,memory.total",
                "--format=csv,noheader,nounits",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        Self {
            gpu: Self::best_gpu(&out),
            kvm: Path::new("/dev/kvm").exists(),
            preempt_rt: RunningKernel::probe().preempt_rt,
            backends: Vec::new(),
            simulators: crate::scenario::available_simulators(),
        }
    }

    /// Parse `nvidia-smi`'s CSV and keep the most capable card — a host with two GPUs qualifies
    /// on the one that can do the work, not on whichever the driver happened to list first.
    /// Pure, so the parsing is testable without a GPU.
    pub(crate) fn best_gpu(csv: &str) -> Option<GpuCapability> {
        csv.lines()
            .filter_map(|line| {
                let (cap, vram) = line.split_once(',')?;
                Some(GpuCapability {
                    compute_cap: cap.trim().parse().ok()?,
                    vram_mib: vram.trim().parse().ok()?,
                })
            })
            .max_by(|a, b| {
                a.compute_cap
                    .total_cmp(&b.compute_cap)
                    .then(a.vram_mib.cmp(&b.vram_mib))
            })
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RequirementError {
    #[error(
        "`{workspace}` requires `gpu: tier2`, which this host does not meet: {found}. Nothing \
         was provisioned. Tier 2 is RTX-class — compute capability >= {TIER2_MIN_COMPUTE_CAP:.1} \
         and >= {TIER2_MIN_VRAM_MIB} MiB VRAM (`TESTBED-SETUP.md`). Compute capability is the \
         binding half: DeepStream/TensorRT need tensor cores, so a pre-Turing card is excluded \
         on architecture rather than memory and more VRAM within that generation does not help. \
         Run `vxedge doctor --tier 2` for the full picture, or run this experiment on a host \
         that qualifies."
    )]
    UnmetGpu { workspace: String, found: String },
    #[error(
        "`{workspace}` declares `requires.gpu: {value}` — expected `none` or `tier2`. Nothing \
         was provisioned, because a requirement nobody can evaluate is not a safe default."
    )]
    UnknownGpu { workspace: String, value: String },
    #[error(
        "`{workspace}` requires `kvm: true`, and this host has no `/dev/kvm`. Nothing was \
         provisioned. Enable virtualization in the firmware and load the kvm module, or run it on \
         a host with KVM."
    )]
    UnmetKvm { workspace: String },
    #[error(
        "`{workspace}` requires a PREEMPT_RT kernel (`kernel.preempt_rt: required`), and this \
         host's kernel is not one. Nothing was provisioned. Boot a real-time kernel, or declare \
         `preferred` if the experiment may run without one and report the kernel as a caveat."
    )]
    UnmetPreemptRt { workspace: String },
    #[error(
        "`{workspace}` requires backends this host cannot run: {missing}. Nothing was \
         provisioned. `HostService.Capabilities` lists what this host runs and why not."
    )]
    UnmetBackends { workspace: String, missing: String },
}

impl Requires {
    /// The simulators this package needs that `host` does not have. Kept apart from
    /// [`Requires::check`]: vxAPI reports them as `ERROR_REASON_SIMULATOR_UNAVAILABLE`.
    pub fn missing_simulators(&self, host: &HostCapabilities) -> Vec<String> {
        self.simulators
            .iter()
            .filter(|s| !host.simulators.contains(s))
            .cloned()
            .collect()
    }

    /// Whether `host` can run this workspace: every unmet requirement, not only the first, so
    /// `Plan` can name them all. Pure.
    pub fn check(
        &self,
        workspace: &str,
        host: &HostCapabilities,
    ) -> Result<(), Vec<RequirementError>> {
        let workspace = workspace.to_string();
        let mut unmet = Vec::new();
        match self.gpu.as_deref() {
            None | Some("none") => {}
            Some("tier2") => match host.gpu {
                Some(gpu) if gpu.meets_tier2() => {}
                Some(gpu) => unmet.push(RequirementError::UnmetGpu {
                    workspace: workspace.clone(),
                    found: gpu.describe(),
                }),
                None => unmet.push(RequirementError::UnmetGpu {
                    workspace: workspace.clone(),
                    found: "no NVIDIA GPU the driver can see".to_string(),
                }),
            },
            Some(other) => unmet.push(RequirementError::UnknownGpu {
                workspace: workspace.clone(),
                value: other.to_string(),
            }),
        }
        if self.kvm && !host.kvm {
            unmet.push(RequirementError::UnmetKvm {
                workspace: workspace.clone(),
            });
        }
        if self.preempt_rt == Some(PreemptRt::Required) && !host.preempt_rt {
            unmet.push(RequirementError::UnmetPreemptRt {
                workspace: workspace.clone(),
            });
        }
        let missing: Vec<String> = self
            .backends
            .iter()
            .filter_map(|b| match host.backends.iter().find(|p| p.backend == *b) {
                Some(BackendProbe {
                    unavailable: None, ..
                }) => None,
                Some(BackendProbe {
                    unavailable: Some(why),
                    ..
                }) => Some(format!("{} ({why})", b.as_str())),
                None => Some(format!("{} (not probed)", b.as_str())),
            })
            .collect();
        if !missing.is_empty() {
            unmet.push(RequirementError::UnmetBackends {
                workspace,
                missing: missing.join(", "),
            });
        }
        if unmet.is_empty() { Ok(()) } else { Err(unmet) }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error(
        "no vxedge experiment workspace found in {}: expected a `vxp.yaml` here. Run `vxedge experiment new <name>` to create one, `cd` into an \
         existing workspace, or point at one with `--chdir <dir>`.",
        .dir.display()
    )]
    NotAWorkspace { dir: PathBuf },
    #[error(
        "{} holds both `vxp.yaml` and {} — a package has one descriptor, `vxp.yaml`. Delete a \
         leftover legacy `<name>.vxp`, or move a packed `.vxp` out of the source folder (it \
         would be an input to its own next pack).",
        .dir.display(), .found.join(", ")
    )]
    BothDescriptors { dir: PathBuf, found: Vec<String> },
    #[error(
        "{} is a packed package, not a descriptor. Run it from a source folder: \
         `vxedge pkg unpack` restores one.",
        .path.display()
    )]
    PackedPackage { path: PathBuf },
    #[error(
        "{} has no `schema`. `vxp.yaml` is the `vxp/1` descriptor: add `schema: vxp/1` as its \
         first line.",
        .path.display()
    )]
    MissingSchema { path: PathBuf },
    #[error(
        "{} declares `schema: {found}`, which this vxedge does not read (it reads {}). \
         Upgrade vxedge, or use a package built for this version.",
        .path.display(), KNOWN_SCHEMAS.join(", ")
    )]
    UnsupportedSchema { path: PathBuf, found: String },
    #[error(
        "{} is a legacy `<name>.vxp` descriptor, which this vxedge no longer reads. The package \
         format is `vxp.yaml` with `schema: vxp/1`: rename it to `vxp.yaml`, add `schema: vxp/1` \
         as its first line, make `id` `namespace/name` and give it a `version` and an `entry`, \
         then run `vxedge pkg lock` to write its `vxp.lock`.",
        .path.display()
    )]
    LegacyDescriptor { path: PathBuf },
    #[error("{} is not a valid package descriptor: {reason}", .path.display())]
    Invalid { path: PathBuf, reason: String },
    // A field named `source` is picked up by thiserror as the error source, so the cause is
    // already printed by the caller walking the chain. Repeating `{source}` here would print
    // it twice.
    #[error("cannot read {}", .path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} is not a valid `vxp.yaml` package descriptor", .path.display())]
    Parse {
        path: PathBuf,
        source: serde_yaml_ng::Error,
    },
}

/// A workspace resolved from a directory.
#[derive(Debug, Clone)]
pub struct DiscoveredWorkspace {
    pub root: PathBuf,
    /// The `vxp.yaml`.
    pub descriptor_path: PathBuf,
    pub package: PackageDoc,
}

/// Find the descriptor in `dir` and read it. Does not read the rest of the workspace —
/// [`crate::Workspace::load`] does that, and keeping the two separate means a broken experiment
/// document still produces a *good* "which workspace is this" error.
pub fn discover(dir: &Path) -> Result<DiscoveredWorkspace, DiscoverError> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| DiscoverError::Io { path, source }
    };
    let mut vxp: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(io(dir))? {
        let path = entry.map_err(io(dir))?.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "vxp") {
            vxp.push(path);
        }
    }
    // Directory order is filesystem-defined; sort so the errors read the same twice.
    vxp.sort();

    let current = dir.join(DESCRIPTOR_FILE);
    if current.is_file() {
        if !vxp.is_empty() {
            return Err(DiscoverError::BothDescriptors {
                dir: dir.to_path_buf(),
                found: vxp
                    .iter()
                    .filter_map(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .collect(),
            });
        }
        let text = std::fs::read_to_string(&current).map_err(io(&current))?;
        let package = read_current(&current, &text)?;
        return Ok(DiscoveredWorkspace {
            root: dir.to_path_buf(),
            descriptor_path: current,
            package,
        });
    }
    // No `vxp.yaml`: say what a `*.vxp` here is, a legacy descriptor first, as that is the one
    // to migrate.
    let mut packed = None;
    for path in vxp {
        let bytes = std::fs::read(&path).map_err(io(&path))?;
        if !is_tar(&bytes) {
            return Err(DiscoverError::LegacyDescriptor { path });
        }
        packed.get_or_insert(path);
    }
    Err(match packed {
        Some(path) => DiscoverError::PackedPackage { path },
        None => DiscoverError::NotAWorkspace {
            dir: dir.to_path_buf(),
        },
    })
}

/// A ustar header carries `ustar` at offset 257 of its first block.
fn is_tar(bytes: &[u8]) -> bool {
    bytes.get(257..262) == Some(b"ustar")
}

/// Read `vxp.yaml`: dispatch on `schema` from the raw mapping, then parse what that schema says.
fn read_current(path: &Path, text: &str) -> Result<PackageDoc, DiscoverError> {
    let parse = |source| DiscoverError::Parse {
        path: path.to_path_buf(),
        source,
    };
    let invalid = |reason: String| DiscoverError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    let serde_yaml_ng::Value::Mapping(mut map) =
        serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text).map_err(parse)?
    else {
        return Err(invalid("expected a mapping of keys".to_string()));
    };
    match map.remove("schema") {
        None => Err(DiscoverError::MissingSchema {
            path: path.to_path_buf(),
        }),
        Some(serde_yaml_ng::Value::String(s)) if s == SCHEMA_V1 => read_v1(map, path),
        Some(serde_yaml_ng::Value::String(found)) => Err(DiscoverError::UnsupportedSchema {
            path: path.to_path_buf(),
            found,
        }),
        Some(other) => Err(invalid(format!(
            "`schema` must be a string such as `{SCHEMA_V1}`, found `{}`",
            serde_yaml_ng::to_string(&other).unwrap_or_default().trim()
        ))),
    }
}

fn read_v1(map: serde_yaml_ng::Mapping, path: &Path) -> Result<PackageDoc, DiscoverError> {
    let invalid = |reason: String| DiscoverError::Invalid {
        path: path.to_path_buf(),
        reason,
    };
    let mut fields = serde_yaml_ng::Mapping::new();
    let mut extensions = BTreeMap::new();
    for (key, value) in map {
        match key.as_str() {
            Some(k) if k.starts_with("x-") => {
                extensions.insert(k.to_string(), value);
            }
            _ => {
                fields.insert(key, value);
            }
        }
    }
    let f: V1Fields =
        serde_yaml_ng::from_value(serde_yaml_ng::Value::Mapping(fields)).map_err(|source| {
            DiscoverError::Parse {
                path: path.to_path_buf(),
                source,
            }
        })?;
    // `entry` names a top-level document: the loader reads only those, and the descriptor is
    // not one.
    if f.entry.contains('/')
        || !f.entry.ends_with(".yaml")
        || f.entry == DESCRIPTOR_FILE
        || f.entry.starts_with('.')
    {
        return Err(invalid(format!(
            "`entry: {}` must name a top-level `*.yaml` document other than `{DESCRIPTOR_FILE}`",
            f.entry
        )));
    }
    Ok(PackageDoc {
        id: f.id,
        version: f.version,
        entry: f.entry,
        title: f.title,
        license: f.license,
        authors: f.authors,
        requires: f.requires,
        provenance: f.provenance,
        extensions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vxpackage-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_vxp(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn an_empty_directory_is_not_a_workspace() {
        let d = tmp("empty");
        match discover(&d) {
            Err(DiscoverError::NotAWorkspace { .. }) => {}
            other => panic!("expected NotAWorkspace, got {other:?}"),
        }
        std::fs::remove_dir_all(&d).ok();
    }

    /// The rule that keeps `up` from re-provisioning a parent experiment when run from
    /// `results/`. Terraform behaves this way; `git` does not, and `git`'s behaviour would be
    /// actively dangerous here.
    #[test]
    fn discovery_does_not_walk_up_to_a_parent_workspace() {
        let d = tmp("nowalk");
        std::fs::write(d.join(DESCRIPTOR_FILE), V1).unwrap();
        let child = d.join("results");
        std::fs::create_dir_all(&child).unwrap();
        match discover(&child) {
            Err(DiscoverError::NotAWorkspace { .. }) => {}
            other => panic!("expected NotAWorkspace from a subdirectory, got {other:?}"),
        }
        std::fs::remove_dir_all(&d).ok();
    }

    const V1: &str = "schema: vxp/1\n\
                      id: vxedge/h1-slice-isolation\n\
                      version: 1.2.0\n\
                      entry: main.yaml\n\
                      title: Slice isolation under background load\n\
                      license: Apache-2.0\n\
                      authors: [{ org: vxedge }]\n\
                      requires:\n  \
                        vxedge: \">=0.6.0, <0.8.0\"\n  \
                        gpu: none\n  \
                        kvm: true\n  \
                        kernel: { preempt_rt: preferred }\n  \
                        backends: [container, qemu-microvm]\n  \
                        simulator: [esmini]\n\
                      provenance:\n  \
                        generated_by: { tool: vxmodel, version: 0.3.0 }\n\
                      x-oem-acme: { cost_centre: 4711 }\n";

    fn v1_with(dir: &Path, body: &str) -> Result<DiscoveredWorkspace, DiscoverError> {
        std::fs::write(dir.join(DESCRIPTOR_FILE), body).unwrap();
        discover(dir)
    }

    /// The `ARCHITECTURE.md` §6.0.3 example, whole: every field read, `x-` keys carried.
    #[test]
    fn a_vxp1_descriptor_is_read_whole() {
        let d = tmp("v1-whole");
        let ws = v1_with(&d, V1).unwrap();
        let v1 = &ws.package;
        assert_eq!(v1.id.as_str(), "vxedge/h1-slice-isolation");
        assert_eq!(v1.experiment_id(), "h1-slice-isolation");
        assert_eq!(v1.entry, "main.yaml");
        assert_eq!(v1.version, semver::Version::new(1, 2, 0));
        assert_eq!(v1.id.namespace(), "vxedge");
        let range = ws.package.vxedge_range().expect("range");
        assert!(range.matches(&semver::Version::new(0, 7, 3)));
        assert!(!range.matches(&semver::Version::new(0, 8, 0)));
        assert_eq!(v1.requires.kvm, Some(true));
        assert_eq!(v1.requires.simulator, ["esmini"]);
        let requires = ws.package.host_requires();
        assert_eq!(
            requires.missing_simulators(&HostCapabilities::default()),
            ["esmini"]
        );
        let with_esmini = HostCapabilities {
            simulators: vec!["esmini".into()],
            ..Default::default()
        };
        assert!(requires.missing_simulators(&with_esmini).is_empty());
        let unknown = v1_with(
            &tmp("v1-sim"),
            &V1.replace("simulator: [esmini]", "simulator: [unity]"),
        );
        assert!(
            format!("{:?}", unknown.unwrap_err()).contains("unknown simulator `unity`"),
            "an unknown simulator is refused"
        );
        assert_eq!(
            v1.requires.backends,
            vec![
                crate::model::Backend::Container,
                crate::model::Backend::QemuMicrovm
            ]
        );
        assert!(v1.extensions.contains_key("x-oem-acme"));
        assert_eq!(ws.descriptor_path, d.join(DESCRIPTOR_FILE));
        std::fs::remove_dir_all(&d).ok();
    }

    /// Within a known schema a typo is a mistake; an `x-` key is an annotation.
    #[test]
    fn vxp1_refuses_unknown_keys_but_carries_x_keys() {
        let d = tmp("v1-unknown");
        let typo = V1.replace("title:", "titel:");
        assert!(matches!(
            v1_with(&d, &typo),
            Err(DiscoverError::Parse { .. })
        ));
        std::fs::remove_dir_all(&d).ok();
    }

    /// A newer schema is a typed refusal naming what this build reads, never a parse error.
    #[test]
    fn an_unknown_schema_is_refused_as_unsupported() {
        let d = tmp("v1-schema");
        let newer = V1.replace("schema: vxp/1", "schema: vxp/9");
        match v1_with(&d, &newer) {
            Err(e @ DiscoverError::UnsupportedSchema { .. }) => {
                let msg = e.to_string();
                assert!(msg.contains("vxp/9") && msg.contains("vxp/1"), "{msg}");
            }
            other => panic!("expected UnsupportedSchema, got {other:?}"),
        }
        std::fs::remove_dir_all(&d).ok();
    }

    /// The file name already declares the new format, so a missing `schema` is a mistake.
    #[test]
    fn vxp_yaml_without_a_schema_is_refused() {
        let d = tmp("v1-noschema");
        let bare = V1.replace("schema: vxp/1\n", "");
        assert!(matches!(
            v1_with(&d, &bare),
            Err(DiscoverError::MissingSchema { .. })
        ));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn vxp1_checks_id_version_range_and_backends() {
        for (what, from, to) in [
            (
                "bare id",
                "id: vxedge/h1-slice-isolation",
                "id: h1-slice-isolation",
            ),
            (
                "upper case",
                "id: vxedge/h1-slice-isolation",
                "id: VxEdge/h1",
            ),
            ("three parts", "id: vxedge/h1-slice-isolation", "id: a/b/c"),
            (
                "leading dash",
                "id: vxedge/h1-slice-isolation",
                "id: vxedge/-h1",
            ),
            ("version", "version: 1.2.0", "version: 1.2"),
            ("range", "\">=0.6.0, <0.8.0\"", "\"at least six\""),
            (
                "backend",
                "[container, qemu-microvm]",
                "[container, firecracker]",
            ),
            ("gpu", "gpu: none", "gpu: tier9"),
            ("preempt_rt", "preempt_rt: preferred", "preempt_rt: maybe"),
            ("entry", "entry: main.yaml", "entry: docs/main.yaml"),
            ("entry", "entry: main.yaml", "entry: vxp.yaml"),
            (
                "author",
                "authors: [{ org: vxedge }]",
                "authors: [{ name: someone }]",
            ),
        ] {
            let d = tmp("v1-bad");
            let body = V1.replace(from, to);
            assert_ne!(body, V1, "{what}: fixture did not change");
            assert!(v1_with(&d, &body).is_err(), "{what}: `{to}` was accepted");
            std::fs::remove_dir_all(&d).ok();
        }
    }

    /// `vxp.yaml` beside a leftover legacy descriptor is two answers to one question.
    #[test]
    fn vxp_yaml_beside_a_legacy_descriptor_is_refused() {
        let d = tmp("v1-both");
        write_vxp(&d, "h1-slice-isolation.vxp", "id: h1-slice-isolation\n");
        match v1_with(&d, V1) {
            Err(e @ DiscoverError::BothDescriptors { .. }) => {
                let msg = e.to_string();
                assert!(msg.contains("h1-slice-isolation.vxp"), "{msg}");
            }
            other => panic!("expected BothDescriptors, got {other:?}"),
        }
        std::fs::remove_dir_all(&d).ok();
    }

    /// A packed package found where a descriptor is looked for is never parsed as YAML.
    #[test]
    fn a_packed_vxp_is_not_read_as_a_descriptor() {
        let d = tmp("packed");
        let mut block = vec![0u8; 512];
        block[..9].copy_from_slice(b"main.yaml");
        block[257..263].copy_from_slice(b"ustar\0");
        std::fs::write(d.join("h1-slice-isolation-1.2.0.vxp"), &block).unwrap();
        match discover(&d) {
            Err(e @ DiscoverError::PackedPackage { .. }) => {
                assert!(e.to_string().contains("pkg unpack"), "{e}");
            }
            other => panic!("expected PackedPackage, got {other:?}"),
        }
        std::fs::remove_dir_all(&d).ok();
    }

    /// A legacy `<name>.vxp` descriptor is no longer read, with or without a `schema`: it is
    /// refused with the steps to migrate it, never loaded or parsed. A packed `.vxp` beside it
    /// does not hide it.
    #[test]
    fn a_legacy_descriptor_is_refused_with_how_to_migrate() {
        for (what, body) in [
            ("bare", "id: exp-002\nversion: 0.1.0\nentry: main.yaml\n"),
            ("schema", "schema: vxp/1\nid: vxedge/h1\n"),
            ("unknown key", "id: x\nrequires:\n  kvm: true\n"),
            ("not yaml", ": : :\n"),
        ] {
            let d = tmp("legacy");
            let mut block = vec![0u8; 512];
            block[257..263].copy_from_slice(b"ustar\0");
            std::fs::write(d.join("a-0.1.0.vxp"), &block).unwrap();
            write_vxp(&d, "exp-002.vxp", body);
            match discover(&d) {
                Err(e @ DiscoverError::LegacyDescriptor { .. }) => {
                    let msg = e.to_string();
                    assert!(
                        msg.contains("exp-002.vxp")
                            && msg.contains("no longer reads")
                            && msg.contains("schema: vxp/1")
                            && msg.contains("vxp.lock"),
                        "{what}: {msg}"
                    );
                }
                other => panic!("{what}: expected LegacyDescriptor, got {other:?}"),
            }
            std::fs::remove_dir_all(&d).ok();
        }
    }

    /// A host that cannot meet Tier 2 must refuse *before* provisioning, not partway through —
    /// that is the whole point of a preflight, and the reason exp-011 can be authored and
    /// committed while no such host exists.
    ///
    /// The GT 1030 case: a mere "is there an NVIDIA GPU" probe passes on such a card and
    /// would let exp-011 start and fail deep inside CARLA. Pascal,
    /// 2 GB — it fails Tier 2 on both compute capability and VRAM.
    #[test]
    fn a_tier2_workspace_is_refused_on_a_host_that_cannot_meet_it() {
        let needs_gpu = Requires {
            gpu: Some("tier2".into()),
            ..Default::default()
        };
        let bare = HostCapabilities::default();
        let gt1030 = HostCapabilities {
            gpu: Some(GpuCapability {
                compute_cap: 6.1,
                vram_mib: 2048,
            }),
            ..Default::default()
        };
        let equipped = HostCapabilities {
            gpu: Some(GpuCapability {
                compute_cap: 8.9,
                vram_mib: 24564,
            }),
            ..Default::default()
        };

        assert!(
            matches!(
                needs_gpu.check("exp-011", &gt1030),
                Err(ref e) if matches!(&e[..], [RequirementError::UnmetGpu { found, .. }] if found.contains("6.1"))
            ),
            "a pre-Turing 2 GB card must not satisfy tier2"
        );
        // Enough VRAM but no tensor cores is still a refusal: the architecture is the binding
        // half, so adding memory within that generation does not qualify a host.
        let big_pascal = HostCapabilities {
            gpu: Some(GpuCapability {
                compute_cap: 6.1,
                vram_mib: 24564,
            }),
            ..Default::default()
        };
        assert!(needs_gpu.check("exp-011", &big_pascal).is_err());

        assert!(matches!(
            needs_gpu.check("exp-011", &bare).unwrap_err()[..],
            [RequirementError::UnmetGpu { .. }]
        ));
        assert!(needs_gpu.check("exp-011", &equipped).is_ok());

        // The common case costs nothing, stated or omitted.
        assert!(Requires::default().check("exp-001", &bare).is_ok());
        assert!(
            Requires {
                gpu: Some("none".into()),
                ..Default::default()
            }
            .check("exp-001", &bare)
            .is_ok()
        );

        // An unrecognised value refuses rather than defaulting to permissive: a requirement
        // nobody can evaluate is not a safe default.
        assert!(matches!(
            Requires {
                gpu: Some("tier9".into()),
                ..Default::default()
            }
            .check("exp-x", &equipped)
            .unwrap_err()[..],
            [RequirementError::UnknownGpu { .. }]
        ));
    }

    /// A two-GPU host qualifies on the card that can do the work, not on whichever the driver
    /// listed first.
    #[test]
    fn the_most_capable_gpu_is_the_one_that_counts() {
        let two = HostCapabilities::best_gpu("6.1, 2048\n8.9, 24564\n").unwrap();
        assert_eq!(two.compute_cap, 8.9);
        assert_eq!(two.vram_mib, 24564);
        // Real `nvidia-smi --format=csv,noheader,nounits` output from the development host.
        let one = HostCapabilities::best_gpu("6.1, 2048\n").unwrap();
        assert_eq!(one.compute_cap, 6.1);
        assert!(HostCapabilities::best_gpu("").is_none());
        assert!(HostCapabilities::best_gpu("no devices found").is_none());
    }

    fn probe(backend: Backend, unavailable: Option<&str>) -> BackendProbe {
        BackendProbe {
            backend,
            unavailable: unavailable.map(str::to_string),
        }
    }

    /// What `vxp/1` declares is what the preflight checks: `kvm`, `kernel.preempt_rt` and
    /// `backends` beside `gpu`.
    #[test]
    fn a_vxp1_descriptor_hands_every_host_requirement_to_the_check() {
        let d = tmp("v1-host-requires");
        let ws = v1_with(&d, V1).unwrap();
        let r = ws.package.host_requires();
        assert_eq!(r.gpu.as_deref(), Some("none"));
        assert!(r.kvm);
        assert_eq!(r.preempt_rt, Some(PreemptRt::Preferred));
        assert_eq!(r.backends, vec![Backend::Container, Backend::QemuMicrovm]);
        std::fs::remove_dir_all(&d).ok();
    }

    /// Each unmet requirement is its own error, all of them reported, so `Plan` can name every
    /// one instead of the first.
    #[test]
    fn every_unmet_host_requirement_is_reported() {
        let needs = Requires {
            kvm: true,
            preempt_rt: Some(PreemptRt::Required),
            backends: vec![Backend::QemuMicrovm, Backend::Libvirt, Backend::Container],
            ..Default::default()
        };
        let host = HostCapabilities {
            backends: vec![
                probe(Backend::QemuMicrovm, Some("needs /dev/kvm")),
                probe(Backend::Container, None),
            ],
            ..Default::default()
        };
        let errors = needs.check("exp-x", &host).unwrap_err();
        assert!(errors.contains(&RequirementError::UnmetKvm {
            workspace: "exp-x".into()
        }));
        assert!(errors.contains(&RequirementError::UnmetPreemptRt {
            workspace: "exp-x".into()
        }));
        let backends = errors
            .iter()
            .find(|e| matches!(e, RequirementError::UnmetBackends { .. }))
            .expect("backends reported")
            .to_string();
        // The probe's reason travels with the name; a backend nobody probed is unavailable, never
        // assumed present.
        assert!(
            backends.contains("qemu-microvm (needs /dev/kvm)"),
            "{backends}"
        );
        assert!(backends.contains("libvirt"), "{backends}");
        assert!(!backends.contains("container"), "{backends}");
        assert_eq!(errors.len(), 3, "{errors:?}");
    }

    #[test]
    fn met_or_soft_host_requirements_pass() {
        let host = HostCapabilities {
            kvm: true,
            preempt_rt: false,
            backends: vec![probe(Backend::Container, None)],
            ..Default::default()
        };
        // `preferred` and `any` run on any kernel; `kvm: false` asks for nothing.
        for preempt_rt in [None, Some(PreemptRt::Preferred), Some(PreemptRt::Any)] {
            let r = Requires {
                kvm: true,
                preempt_rt,
                backends: vec![Backend::Container],
                ..Default::default()
            };
            assert_eq!(r.check("exp-x", &host), Ok(()));
        }
        assert_eq!(
            Requires::default().check("exp-x", &HostCapabilities::default()),
            Ok(())
        );
        let rt = HostCapabilities {
            preempt_rt: true,
            ..Default::default()
        };
        let required = Requires {
            preempt_rt: Some(PreemptRt::Required),
            ..Default::default()
        };
        assert_eq!(required.check("exp-x", &rt), Ok(()));
    }

    /// `/sys/kernel/realtime` decides when the kernel has it; older real-time kernels only say so
    /// in their version string.
    #[test]
    fn taprio_is_found_loaded_built_in_or_installed() {
        let dep = "kernel/net/sched/sch_htb.ko.zst:\nkernel/net/sched/sch_taprio.ko.zst:\n";
        assert!(taprio_available(false, None, Some(dep)));
        assert!(taprio_available(
            false,
            Some("kernel/net/sched/sch_taprio.ko\n"),
            None
        ));
        assert!(taprio_available(true, None, None));
        assert!(taprio_available(
            false,
            None,
            Some("kernel/net/sched/sch_taprio.ko: kernel/net/sched/sch_mqprio_lib.ko\n")
        ));
        // A neighbour's name is not taprio's.
        assert!(!taprio_available(
            false,
            Some("kernel/net/sched/sch_mqprio.ko\n"),
            Some("kernel/net/sched/sch_htb.ko.zst:\nkernel/net/sched/sch_taprio_x.ko:\n")
        ));
        assert!(!taprio_available(false, None, None));
    }

    #[test]
    fn a_preempt_rt_kernel_is_recognised() {
        assert!(kernel_is_preempt_rt(Some("1\n"), ""));
        assert!(!kernel_is_preempt_rt(Some("0\n"), "#1 SMP PREEMPT_RT"));
        assert!(kernel_is_preempt_rt(
            None,
            "#1 SMP PREEMPT_RT Fri Jun  6 10:00:00 UTC 2025"
        ));
        assert!(!kernel_is_preempt_rt(
            None,
            "#42-Ubuntu SMP PREEMPT_DYNAMIC Tue Oct 14 2026"
        ));
    }
}
