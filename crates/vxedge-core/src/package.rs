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

/// What a workspace needs from its host. Parsed here; **enforced** by the `up` preflight
/// (`TASKS.md` section 0 task 4), which is what lets a GPU-dependent experiment be authored,
/// validated and committed while remaining inert.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    /// `none` | `tier2`. Left as a string until the preflight gives the values behaviour —
    /// an enum whose variants nothing reads would just be a second place to edit.
    #[serde(default)]
    pub gpu: Option<String>,
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
}
