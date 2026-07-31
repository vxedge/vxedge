//! The `.vxp` package descriptor, and discovering a workspace from a directory.
//!
//! A workspace is addressed the way Terraform addresses a configuration: **the working
//! directory is the unit of work**. `vxedge experiment up` takes no name — it looks for exactly
//! one `.vxp` in the directory it was run from.
//!
//! Two properties are deliberate, and both are Terraform's:
//!
//! 1. **No walking up to parent directories.** `git` searches upward; Terraform does not, and
//!    neither do we. Running from inside `results/` must fail rather than silently resolve the
//!    parent experiment and re-provision it — the same reasoning that made
//!    [`crate::Workspace::load`] non-recursive.
//! 2. **Exactly one.** Zero and many are both errors, and the "many" error names what it found,
//!    because the fix depends on which files are there.
//!
//! The descriptor is separate from `main.yaml` so a version bump never touches experiment
//! content — the identity of a package and the content of an experiment change for different
//! reasons and at different times.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A `.vxp` package descriptor: what this workspace *is*, as distinct from what it *does*.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageDoc {
    pub id: String,
    #[serde(default)]
    pub version: Option<String>,
    /// The experiment document, when the workspace wants to be explicit. Discovery does not
    /// need it — files are routed by `kind`, not by path (§6.0) — so it stays advisory.
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub requires: Requires,
}

/// What a workspace needs from its host, checked by [`Requires::check`] before `up` provisions
/// anything.
///
/// This is what lets a GPU-dependent experiment be authored, validated and committed while
/// remaining inert: `exp-011` can exist as a sandbox before any Tier 2 host does, and the
/// unresolved GPU decision blocks nothing else. The check runs **daemon-side**, against the
/// host that would do the provisioning — a CLI-side check would be asking the wrong machine as
/// soon as `--endpoint` points somewhere else.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    /// `none` | `tier2`, per `TESTBED-SETUP.md`'s host tiers.
    #[serde(default)]
    pub gpu: Option<String>,
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
    /// Both, not merely "a GPU is present": the development host this was first run on has a
    /// GT 1030 — a real NVIDIA GPU, and a *Pascal* one with 2 GB, so it fails Tier 2 on both
    /// counts. A presence check would have let `exp-011` start there and fail deep inside
    /// CARLA, which is exactly what the preflight exists to prevent.
    pub gpu: Option<GpuCapability>,
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
    /// Probe the host this process runs on.
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
}

impl Requires {
    /// Whether `host` can run this workspace. Pure.
    pub fn check(&self, workspace: &str, host: &HostCapabilities) -> Result<(), RequirementError> {
        match self.gpu.as_deref() {
            None | Some("none") => Ok(()),
            Some("tier2") => match host.gpu {
                Some(gpu) if gpu.meets_tier2() => Ok(()),
                Some(gpu) => Err(RequirementError::UnmetGpu {
                    workspace: workspace.to_string(),
                    found: gpu.describe(),
                }),
                None => Err(RequirementError::UnmetGpu {
                    workspace: workspace.to_string(),
                    found: "no NVIDIA GPU the driver can see".to_string(),
                }),
            },
            Some(other) => Err(RequirementError::UnknownGpu {
                workspace: workspace.to_string(),
                value: other.to_string(),
            }),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error(
        "no vxedge experiment workspace found in {}: expected a `.vxp` file here. \
         Run `vxedge experiment new <name>` to create one, `cd` into an existing workspace, \
         or point at one with `--chdir <dir>`.",
        .dir.display()
    )]
    NotAWorkspace { dir: PathBuf },
    #[error(
        "{} contains {} `.vxp` files ({}) — a workspace is exactly one experiment. \
         Split them into separate directories, or move the extras out.",
        .dir.display(), .found.len(), .found.join(", ")
    )]
    Ambiguous { dir: PathBuf, found: Vec<String> },
    // A field named `source` is picked up by thiserror as the error source, so the cause is
    // already printed by the caller walking the chain. Repeating `{source}` here would print
    // it twice.
    #[error("cannot read {}", .path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} is not a valid `.vxp` package descriptor", .path.display())]
    Parse {
        path: PathBuf,
        source: serde_yaml_ng::Error,
    },
}

/// A workspace resolved from a directory.
#[derive(Debug, Clone)]
pub struct DiscoveredWorkspace {
    pub root: PathBuf,
    pub vxp_path: PathBuf,
    pub package: PackageDoc,
}

/// Find the single `.vxp` in `dir` and read it. Does not read the rest of the workspace —
/// [`crate::Workspace::load`] does that, and keeping the two separate means a broken experiment
/// document still produces a *good* "which workspace is this" error.
pub fn discover(dir: &Path) -> Result<DiscoveredWorkspace, DiscoverError> {
    let entries = std::fs::read_dir(dir).map_err(|source| DiscoverError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    let mut found: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| DiscoverError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|e| e == "vxp") {
            found.push(path);
        }
    }
    // Directory order is filesystem-defined; sort so the "many" error reads the same twice.
    found.sort();

    let vxp_path = match found.len() {
        0 => {
            return Err(DiscoverError::NotAWorkspace {
                dir: dir.to_path_buf(),
            });
        }
        1 => found.remove(0),
        _ => {
            return Err(DiscoverError::Ambiguous {
                dir: dir.to_path_buf(),
                found: found
                    .iter()
                    .filter_map(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .collect(),
            });
        }
    };

    let text = std::fs::read_to_string(&vxp_path).map_err(|source| DiscoverError::Io {
        path: vxp_path.clone(),
        source,
    })?;
    let package: PackageDoc =
        serde_yaml_ng::from_str(&text).map_err(|source| DiscoverError::Parse {
            path: vxp_path.clone(),
            source,
        })?;

    Ok(DiscoveredWorkspace {
        root: dir.to_path_buf(),
        vxp_path,
        package,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vxpkg-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_vxp(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn discovers_the_single_vxp() {
        let d = tmp("single");
        write_vxp(
            &d,
            "demo.vxp",
            "id: demo\nversion: 0.1.0\nrequires:\n  gpu: none\n",
        );
        let ws = discover(&d).unwrap();
        assert_eq!(ws.package.id, "demo");
        assert_eq!(ws.package.requires.gpu.as_deref(), Some("none"));
        std::fs::remove_dir_all(&d).ok();
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

    /// Two `.vxp` files is ambiguous, and the error has to name them — which one to remove is
    /// the engineer's call, and they cannot make it from a count.
    #[test]
    fn two_vxp_files_are_ambiguous_and_named() {
        let d = tmp("two");
        write_vxp(&d, "a.vxp", "id: a\n");
        write_vxp(&d, "b.vxp", "id: b\n");
        match discover(&d) {
            Err(e @ DiscoverError::Ambiguous { .. }) => {
                let msg = e.to_string();
                assert!(msg.contains("a.vxp") && msg.contains("b.vxp"), "{msg}");
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        std::fs::remove_dir_all(&d).ok();
    }

    /// The rule that keeps `up` from re-provisioning a parent experiment when run from
    /// `results/`. Terraform behaves this way; `git` does not, and `git`'s behaviour would be
    /// actively dangerous here.
    #[test]
    fn discovery_does_not_walk_up_to_a_parent_workspace() {
        let d = tmp("nowalk");
        write_vxp(&d, "parent.vxp", "id: parent\n");
        let child = d.join("results");
        std::fs::create_dir_all(&child).unwrap();
        match discover(&child) {
            Err(DiscoverError::NotAWorkspace { .. }) => {}
            other => panic!("expected NotAWorkspace from a subdirectory, got {other:?}"),
        }
        std::fs::remove_dir_all(&d).ok();
    }

    /// A typo'd key in `.vxp` is a mistake, not an extension point.
    #[test]
    fn unknown_fields_are_rejected() {
        let d = tmp("unknown");
        write_vxp(&d, "x.vxp", "id: x\nrequries:\n  gpu: none\n");
        match discover(&d) {
            Err(DiscoverError::Parse { .. }) => {}
            other => panic!("expected Parse, got {other:?}"),
        }
        std::fs::remove_dir_all(&d).ok();
    }

    /// A host that cannot meet Tier 2 must refuse *before* provisioning, not partway through —
    /// that is the whole point of a preflight, and the reason exp-011 can be authored and
    /// committed while no such host exists.
    ///
    /// The GT 1030 case is real: the development host has one, so a mere "is there an NVIDIA
    /// GPU" probe passed and would have let exp-011 start and fail deep inside CARLA. Pascal,
    /// 2 GB — it fails Tier 2 on both compute capability and VRAM.
    #[test]
    fn a_tier2_workspace_is_refused_on_a_host_that_cannot_meet_it() {
        let needs_gpu = Requires {
            gpu: Some("tier2".into()),
        };
        let bare = HostCapabilities { gpu: None };
        let gt1030 = HostCapabilities {
            gpu: Some(GpuCapability {
                compute_cap: 6.1,
                vram_mib: 2048,
            }),
        };
        let equipped = HostCapabilities {
            gpu: Some(GpuCapability {
                compute_cap: 8.9,
                vram_mib: 24564,
            }),
        };

        assert!(
            matches!(
                needs_gpu.check("exp-011", &gt1030),
                Err(RequirementError::UnmetGpu { ref found, .. }) if found.contains("6.1")
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
        };
        assert!(needs_gpu.check("exp-011", &big_pascal).is_err());

        assert!(matches!(
            needs_gpu.check("exp-011", &bare),
            Err(RequirementError::UnmetGpu { .. })
        ));
        assert!(needs_gpu.check("exp-011", &equipped).is_ok());

        // The common case costs nothing, stated or omitted.
        assert!(Requires::default().check("exp-001", &bare).is_ok());
        assert!(
            Requires {
                gpu: Some("none".into())
            }
            .check("exp-001", &bare)
            .is_ok()
        );

        // An unrecognised value refuses rather than defaulting to permissive: a requirement
        // nobody can evaluate is not a safe default.
        assert!(matches!(
            Requires {
                gpu: Some("tier9".into())
            }
            .check("exp-x", &equipped),
            Err(RequirementError::UnknownGpu { .. })
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
}
