//! Pulls a published, digest-pinned part ref (`ghcr.io/vxedge/parts/...` + `sha256:...`) into
//! a local, content-addressed cache directory, via the `oras` CLI.
//!
//! `docs/ARCHITECTURE.md` §6.0's "local-first" rule gives every ref a local form and a
//! published form, resolved by shape — but until this module, nothing on the resolving side
//! of that rule could actually turn a published form into bytes on disk: every call site that
//! received one (`NosCheckSvc::resolve`, `ZoneService`/`ComputeService`'s `PushImage`) refused
//! it outright, naming `docs/TASKS.md` #19. This is the first implementation, wired into
//! `NosCheckSvc::resolve` first because it was the one call site that already had a concrete,
//! real published ref to pull (`ghcr.io/vxedge/parts/gw/nos/os/sonic-vs`) rather than a
//! speculative one. `zones::node_up` (`crates/vxedge-core/src/zones/backend.rs`) is the second:
//! a `NodePlan.remote_bundle` (populated by `zones::plan_nodes_from_model` for a published,
//! non-local bundle part) is fetched here immediately before boot, same mechanism, same cache.
//!
//! Shells out to `oras` rather than adding an OCI-registry crate dependency: `vxparts`' own
//! publish scripts already use the `oras` CLI as this project's one registry client
//! (`publish-*.sh`), and reusing it means there is exactly one implementation of the OCI
//! distribution protocol in play, not two that could disagree.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum RegistryFetchError {
    #[error("cannot execute `oras`: {0} (is the oras CLI installed? https://oras.land)")]
    Spawn(std::io::Error),
    #[error("`oras pull {reference}` failed: {stderr}")]
    Pull { reference: String, stderr: String },
    #[error(
        "digest `{digest}` is not a well-formed `sha256:<64 hex chars>` digest — published \
         refs must be digest-pinned (§13), and a malformed one can't be cache-keyed safely"
    )]
    MalformedDigest { digest: String },
    #[error("{op} `{}` failed: {source}", path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

/// `$XDG_CACHE_HOME/vxedge/parts`, falling back to `$HOME/.cache/vxedge/parts`.
///
/// Deliberately not a subdirectory of `vxedged`'s existing `--runtime-dir` (default
/// `/run/vxedge`): that default is tmpfs, wiped on every reboot, which is right for ephemeral
/// state (audit log, per-experiment staging dirs) and wrong for a pulled, digest-addressed
/// artifact — the whole point of content-addressing it is that it never needs re-pulling once
/// present, and a 6+ GB image (SONiC-VS) re-fetched after every reboot would defeat that.
pub fn default_cache_dir() -> Option<PathBuf> {
    if let Ok(x) = std::env::var("XDG_CACHE_HOME")
        && !x.is_empty()
    {
        return Some(PathBuf::from(x).join("vxedge/parts"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".cache/vxedge/parts"))
}

/// Where a given digest's pulled files would live, without pulling anything. Callers use this
/// to check "is it already cached" without going through [`fetch`].
pub fn cache_path(cache_dir: &Path, digest: &str) -> Result<PathBuf, RegistryFetchError> {
    let hex = digest
        .strip_prefix("sha256:")
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| RegistryFetchError::MalformedDigest {
            digest: digest.to_string(),
        })?;
    Ok(cache_dir.join(hex))
}

/// Pulls `reference` (a bare `ghcr.io/...` ref, e.g. `ghcr.io/vxedge/parts/gw/nos/os/sonic-vs`
/// — no tag or digest suffix; this function pulls by `digest` alone, since that is the only
/// form §13 allows for a published ref) at `digest` into `cache_dir`, returning the directory
/// its files land in.
///
/// Idempotent and blocking (run it behind `tokio::task::spawn_blocking` from an async caller,
/// matching this codebase's existing pattern for `gateway::Shell` calls). A second `fetch()`
/// for the same digest is a no-op: [`cache_path`]'s directory existing at all is treated as
/// "already pulled, complete" — safe only because a pull that fails or is interrupted midway
/// is staged into a sibling temp directory and atomically renamed into place on success, so a
/// half-written pull can never be mistaken for a complete one.
pub fn fetch(
    reference: &str,
    digest: &str,
    cache_dir: &Path,
) -> Result<PathBuf, RegistryFetchError> {
    let dest = cache_path(cache_dir, digest)?;
    if dest.is_dir() {
        return Ok(dest);
    }
    std::fs::create_dir_all(cache_dir).map_err(|source| RegistryFetchError::Io {
        op: "create_dir_all",
        path: cache_dir.to_path_buf(),
        source,
    })?;

    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    let tmp = cache_dir.join(format!(".{hex}.partial"));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp).map_err(|source| RegistryFetchError::Io {
            op: "remove_dir_all (stale partial pull)",
            path: tmp.clone(),
            source,
        })?;
    }
    std::fs::create_dir_all(&tmp).map_err(|source| RegistryFetchError::Io {
        op: "create_dir_all",
        path: tmp.clone(),
        source,
    })?;

    let pull_ref = format!("{reference}@{digest}");
    let output = Command::new("oras")
        .args(["pull", &pull_ref, "-o"])
        .arg(&tmp)
        .output()
        .map_err(RegistryFetchError::Spawn)?;
    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(RegistryFetchError::Pull {
            reference: pull_ref,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    std::fs::rename(&tmp, &dest).map_err(|source| RegistryFetchError::Io {
        op: "rename (stage pulled artifact into place)",
        path: dest.clone(),
        source,
    })?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_path_rejects_malformed_digest() {
        let dir = PathBuf::from("/tmp/whatever");
        assert!(cache_path(&dir, "not-a-digest").is_err());
        assert!(cache_path(&dir, "sha256:tooshort").is_err());
        assert!(cache_path(&dir, "md5:d41d8cd98f00b204e9800998ecf8427e").is_err());
    }

    #[test]
    fn cache_path_accepts_well_formed_digest() {
        let dir = PathBuf::from("/tmp/whatever");
        let digest = "sha256:0a95ffa745e8e9871653d8605f372a2761edcef2ec53844a37651e343be7dda5";
        let p = cache_path(&dir, digest).unwrap();
        assert_eq!(
            p,
            dir.join("0a95ffa745e8e9871653d8605f372a2761edcef2ec53844a37651e343be7dda5")
        );
    }

    #[test]
    fn fetch_is_noop_when_already_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let digest = "sha256:0a95ffa745e8e9871653d8605f372a2761edcef2ec53844a37651e343be7dda5";
        let dest = cache_path(tmp.path(), digest).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("sentinel"), b"already here").unwrap();

        // No `oras` binary needs to exist for this path: fetch() must return before ever
        // spawning a subprocess when the digest is already cached.
        let got = fetch("ghcr.io/example/does-not-matter", digest, tmp.path()).unwrap();
        assert_eq!(got, dest);
        assert!(dest.join("sentinel").exists());
    }
}
