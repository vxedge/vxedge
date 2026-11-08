//! **Distribution** (`ARCHITECTURE.md` §6.0.8): a packed `vxp/1` package as one OCI artifact.
//!
//! The artifact's single layer is the packed file, unchanged, so the layer digest is the package
//! digest; that is why the packed form is uncompressed. A pull therefore names the package digest
//! and fetches that blob directly: no manifest lookup, no tag. The config blob carries the
//! package's id, version and the parts and images its lock lists, so a registry listing says what
//! a package needs without downloading it.
//!
//! The registry client is the `oras` CLI, as for parts ([`crate::registry`]): one implementation
//! of the distribution protocol in this project, and no registry credentials held by vxedge.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::pkg::{self, Lock, LockedRef, PkgError};

/// The OCI artifact type of a packed package.
pub const ARTIFACT_TYPE: &str = "application/vnd.vxedge.vxp.v1";
/// The media type of its one layer: the packed file, uncompressed.
pub const LAYER_MEDIA_TYPE: &str = "application/vnd.vxedge.vxp.v1.tar";
/// The media type of its config blob ([`ArtifactConfig`]).
pub const CONFIG_MEDIA_TYPE: &str = "application/vnd.vxedge.vxp.config.v1+json";

#[derive(Debug, thiserror::Error)]
pub enum DistributionError {
    #[error(transparent)]
    Package(#[from] PkgError),
    #[error(
        "`{0}` is not a repository: name it without a tag or digest, such as \
         `ghcr.io/acme/experiments/h1-slice-isolation`"
    )]
    NotARepository(String),
    #[error(
        "`{0}` does not name a package digest: pull `<repository>@sha256:<hex>`, the digest \
         `pkg pack` printed. A tag can move, and a result cites what ran."
    )]
    NoDigest(String),
    #[error("cannot run `oras`: {0} (is the oras CLI installed? https://oras.land)")]
    Spawn(std::io::Error),
    #[error("`oras {verb}` failed for `{reference}`: {stderr}")]
    Oras {
        verb: &'static str,
        reference: String,
        stderr: String,
    },
    #[error(
        "the registry returned bytes for `{expected}` that hash to `{actual}`; nothing was kept"
    )]
    DigestMismatch { expected: String, actual: String },
    #[error("{op} `{}`: {source}", .path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

fn io<'a>(
    op: &'static str,
    path: &'a Path,
) -> impl FnOnce(std::io::Error) -> DistributionError + 'a {
    move |source| DistributionError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

/// The config blob of a pushed package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactConfig {
    pub id: String,
    pub version: String,
    pub parts: Vec<LockedRef>,
    pub images: Vec<LockedRef>,
}

/// The config of the packed package `bytes`, read without unpacking it. The packed file must be
/// canonical; a package without a lock lists no parts or images.
pub fn artifact_config(bytes: &[u8]) -> Result<ArtifactConfig, DistributionError> {
    let (descriptor, lock) = pkg::packed_descriptor_and_lock(bytes)?;
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&descriptor)
        .map_err(|e| PkgError::Lock(format!("the packed descriptor does not parse: {e}")))?;
    let field = |k: &str| {
        doc.get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| PkgError::Lock(format!("the packed descriptor has no `{k}`")))
    };
    let lock = lock.map(|text| Lock::from_yaml(&text)).transpose()?;
    Ok(ArtifactConfig {
        id: field("id")?,
        version: field("version")?,
        parts: lock.as_ref().map(|l| l.parts.clone()).unwrap_or_default(),
        images: lock.map(|l| l.images).unwrap_or_default(),
    })
}

/// A repository reference has a registry host and a path, and no tag or digest.
fn check_repository(repository: &str) -> Result<(), DistributionError> {
    let last = repository.rsplit('/').next().unwrap_or("");
    let ok = repository.contains('/')
        && !repository.contains('@')
        && !last.contains(':')
        && !repository.contains(char::is_whitespace);
    ok.then_some(())
        .ok_or_else(|| DistributionError::NotARepository(repository.to_string()))
}

/// The tag a version is pushed under. OCI tags allow no `+`, so SemVer build metadata uses `_`.
pub fn tag_for(version: &str) -> String {
    version.replace('+', "_")
}

/// What [`push`] published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pushed {
    /// `<repository>:<tag>`.
    pub reference: String,
    /// The layer digest, which is the package digest: what [`pull`] takes.
    pub package_digest: String,
}

/// Push the packed package at `file` to `repository`, tagged with its version. The packed file
/// is checked to be canonical first, so a registry never holds a layer `pkg unpack` would refuse.
pub fn push(file: &Path, repository: &str) -> Result<Pushed, DistributionError> {
    check_repository(repository)?;
    let bytes = std::fs::read(file).map_err(io("reading", file))?;
    pkg::read_packed(&bytes)?;
    let config = artifact_config(&bytes)?;
    let package_digest = pkg::sha256(&bytes);

    let stage = std::env::temp_dir().join(format!(
        "vxedge-push-{}-{}",
        std::process::id(),
        package_digest.trim_start_matches("sha256:")
    ));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(io("creating", &stage))?;
    let name = format!(
        "{}-{}.vxp",
        config.id.rsplit('/').next().unwrap_or(&config.id),
        config.version
    );
    let staged = stage.join(&name);
    std::fs::write(&staged, &bytes).map_err(io("writing", &staged))?;
    let config_file = stage.join("config.json");
    let config_json = serde_json::to_vec(&config).expect("the config always serializes");
    std::fs::write(&config_file, config_json).map_err(io("writing", &config_file))?;

    let reference = format!("{repository}:{}", tag_for(&config.version));
    // Relative paths, run from the staging folder: `oras` records each file's name as its title.
    let output = Command::new("oras")
        .current_dir(&stage)
        .args([
            "push",
            &reference,
            "--artifact-type",
            ARTIFACT_TYPE,
            "--config",
        ])
        .arg(format!("config.json:{CONFIG_MEDIA_TYPE}"))
        .arg(format!("{name}:{LAYER_MEDIA_TYPE}"))
        .output()
        .map_err(DistributionError::Spawn);
    let _ = std::fs::remove_dir_all(&stage);
    let output = output?;
    if !output.status.success() {
        return Err(DistributionError::Oras {
            verb: "push",
            reference,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(Pushed {
        reference,
        package_digest,
    })
}

/// `<repository>@sha256:<hex>` split, refusing anything that names no digest.
pub fn split_pull_ref(reference: &str) -> Result<(&str, &str), DistributionError> {
    let (repository, digest) = reference
        .split_once('@')
        .ok_or_else(|| DistributionError::NoDigest(reference.to_string()))?;
    crate::registry::cache_path(Path::new("."), digest)
        .map_err(|_| DistributionError::NoDigest(reference.to_string()))?;
    check_repository(repository)?;
    Ok((repository, digest))
}

/// Where a pulled package with `digest` lives in `cache_dir`: `packages/<hex>.vxp`, beside the
/// pulled parts.
pub fn package_cache_path(cache_dir: &Path, digest: &str) -> Result<PathBuf, DistributionError> {
    let hex = crate::registry::cache_path(Path::new(""), digest)
        .map_err(|_| DistributionError::NoDigest(digest.to_string()))?;
    Ok(cache_dir
        .join("packages")
        .join(format!("{}.vxp", hex.display())))
}

/// Pull the package `reference` (`<repository>@sha256:<hex>`, the package digest) into
/// `cache_dir`, returning the cached packed file. The bytes must hash to the digest and be a
/// canonical packed package, or nothing is kept. A package already in the cache is not fetched
/// again.
pub fn pull(reference: &str, cache_dir: &Path) -> Result<PathBuf, DistributionError> {
    let (repository, digest) = split_pull_ref(reference)?;
    let dest = package_cache_path(cache_dir, digest)?;
    if let Ok(bytes) = std::fs::read(&dest)
        && pkg::sha256(&bytes) == digest
    {
        return Ok(dest);
    }
    let dir = dest.parent().expect("a cache path has a parent");
    std::fs::create_dir_all(dir).map_err(io("creating", dir))?;
    let partial = dest.with_extension("vxp.partial");
    let _ = std::fs::remove_file(&partial);
    let blob = format!("{repository}@{digest}");
    let output = Command::new("oras")
        .args(["blob", "fetch", "--output"])
        .arg(&partial)
        .arg(&blob)
        .output()
        .map_err(DistributionError::Spawn)?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&partial);
        return Err(DistributionError::Oras {
            verb: "blob fetch",
            reference: blob,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let bytes = std::fs::read(&partial).map_err(io("reading", &partial))?;
    let actual = pkg::sha256(&bytes);
    if actual != digest {
        let _ = std::fs::remove_file(&partial);
        return Err(DistributionError::DigestMismatch {
            expected: digest.to_string(),
            actual,
        });
    }
    if let Err(e) = pkg::read_packed(&bytes) {
        let _ = std::fs::remove_file(&partial);
        return Err(e.into());
    }
    std::fs::rename(&partial, &dest).map_err(io("moving into the cache", &dest))?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:1888e766b259786d5d065a4a7c85f3d3ddc6bb589a30cb257156b85e9155a5c6";

    fn packed(lock: Option<&str>) -> Vec<u8> {
        let mut entries = vec![
            (
                "main.yaml".to_string(),
                b"kind: experiment\nid: demo\n".to_vec(),
            ),
            (
                "vxp.yaml".to_string(),
                b"schema: vxp/1\nid: acme/demo\nversion: 1.2.0\nentry: main.yaml\n".to_vec(),
            ),
        ];
        if let Some(l) = lock {
            entries.push(("vxp.lock".to_string(), l.as_bytes().to_vec()));
        }
        entries.sort();
        pkg::pack_entries(&entries).unwrap()
    }

    #[test]
    fn the_config_names_the_package_and_what_its_lock_needs() {
        let lock = "schema: vxp-lock/1\npackage: { id: acme/demo, version: 1.2.0 }\nlocked_with: 0.10.0\n\
                    files: {}\nparts:\n- { ref: ghcr.io/vxedge/parts/x, digest: \"sha256:aa\" }\nimages: []\nvendored: []\n";
        let config = artifact_config(&packed(Some(lock))).unwrap();
        assert_eq!(
            (config.id.as_str(), config.version.as_str()),
            ("acme/demo", "1.2.0")
        );
        assert_eq!(config.parts.len(), 1);
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["parts"][0]["ref"], "ghcr.io/vxedge/parts/x");
        let unlocked = artifact_config(&packed(None)).unwrap();
        assert!(unlocked.parts.is_empty() && unlocked.images.is_empty());
    }

    #[test]
    fn a_pull_names_a_digest_and_a_push_names_a_repository() {
        assert_eq!(
            split_pull_ref(&format!("ghcr.io/acme/demo@{DIGEST}")).unwrap(),
            ("ghcr.io/acme/demo", DIGEST)
        );
        for bad in [
            "ghcr.io/acme/demo:1.2.0",
            "ghcr.io/acme/demo",
            "ghcr.io/acme/demo@sha256:abc",
        ] {
            assert!(
                matches!(split_pull_ref(bad), Err(DistributionError::NoDigest(_))),
                "{bad}"
            );
        }
        assert!(check_repository("ghcr.io/acme/demo").is_ok());
        assert!(
            check_repository("localhost:5000/acme/demo").is_ok(),
            "a port is not a tag"
        );
        for bad in [
            "ghcr.io/acme/demo:1.2.0",
            "demo",
            &format!("ghcr.io/acme/demo@{DIGEST}"),
        ] {
            assert!(check_repository(bad).is_err(), "{bad}");
        }
        assert_eq!(tag_for("1.2.0+lab.3"), "1.2.0_lab.3");
    }

    #[test]
    fn a_pulled_package_is_cached_by_its_digest() {
        let cache = Path::new("/cache");
        assert_eq!(
            package_cache_path(cache, DIGEST).unwrap(),
            cache.join(format!(
                "packages/{}.vxp",
                DIGEST.trim_start_matches("sha256:")
            ))
        );
    }
}
