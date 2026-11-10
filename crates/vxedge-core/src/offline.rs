//! **Offline transfer** (`ARCHITECTURE.md` §6.0.8, decision X-12): a package's published parts and
//! images, carried beside the packed file for an air-gapped host.
//!
//! `pkg pack --offline` copies every published part and image the lock lists into an OCI image
//! layout, `<name>-<version>.parts/`, with `oras copy`, each under the tag `sha256-<hex>` of its
//! digest, and writes [`INDEX_FILE`] naming each one's ref, digest and kind. The packed file and
//! its digest are unchanged. On the air-gapped host, `pkg pull --layout` loads parts into the part
//! cache, where `up` looks for them, and images into the Podman store under the ref and digest the
//! documents use, with `skopeo copy --preserve-digests`, so `up` needs no network.
//!
//! A multi-platform image is carried whole, so a lab on another instruction set has its platform;
//! the host loads its own platform, and Podman resolves the index digest the documents pin.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::pkg::{Lock, LockedRef};

/// The file in a layout that says what each tagged manifest is.
pub const INDEX_FILE: &str = "vxedge-parts.json";

#[derive(Debug, thiserror::Error)]
pub enum OfflineError {
    #[error(
        "these references are pinned by tag only, so an offline copy could not say which bytes \
         ran: {}. Pin each with its digest (`ref@sha256:…` or `digest:`), re-lock, and pack again.",
        .0.join(", ")
    )]
    Unpinned(Vec<String>),
    #[error("cannot run `{tool}`: {source} (is it installed and on PATH?)")]
    Spawn {
        tool: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("`{tool}` failed for `{reference}`: {stderr}")]
    Tool {
        tool: &'static str,
        reference: String,
        stderr: String,
    },
    #[error("`{}` is not an offline layout written by `pkg pack --offline`: {detail}", .layout.display())]
    NotALayout { layout: PathBuf, detail: String },
    #[error("{op} `{}`: {source}", .path.display())]
    Io {
        op: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
}

fn io<'a>(op: &'static str, path: &'a Path) -> impl FnOnce(std::io::Error) -> OfflineError + 'a {
    move |source| OfflineError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

/// What a carried manifest is: a part for the part cache, or an image for the Podman store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Part,
    Image,
}

/// One carried manifest, as [`INDEX_FILE`] lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    #[serde(rename = "ref")]
    pub reference: String,
    pub digest: String,
    pub kind: Kind,
}

impl Entry {
    /// The tag it is stored under in the layout.
    pub fn tag(&self) -> String {
        self.digest.replace(':', "-")
    }
}

/// The layout beside a packed file: `<name>-<version>.parts`.
pub fn layout_path(packed: &Path) -> PathBuf {
    packed.with_extension("parts")
}

/// The images vxedge itself runs on a host, whatever the package declares: the infra container of
/// every container node and 5G group ([`crate::containers::spec::PAUSE_IMAGE`]). They travel with
/// every package, since the lock lists only what the documents name. Only their Linux platforms
/// travel: the pause index also lists Windows images of several hundred megabytes, which no
/// vxedge host runs. The index itself travels unchanged, so its digest still resolves.
pub const RUNTIME_IMAGES: &[&str] = &[crate::containers::spec::PAUSE_IMAGE];

/// What must travel: every published part and image the lock lists, each with its digest, then
/// vxedge's own [`RUNTIME_IMAGES`].
pub fn entries(lock: &Lock) -> Result<Vec<Entry>, OfflineError> {
    let mut unpinned = Vec::new();
    let mut out = Vec::new();
    let mut add = |refs: &[LockedRef], kind| {
        for r in refs {
            match &r.digest {
                Some(digest) => out.push(Entry {
                    reference: r.reference.clone(),
                    digest: digest.clone(),
                    kind,
                }),
                None => unpinned.push(r.reference.clone()),
            }
        }
    };
    add(&lock.parts, Kind::Part);
    add(&lock.images, Kind::Image);
    if !unpinned.is_empty() {
        return Err(OfflineError::Unpinned(unpinned));
    }
    for pinned in RUNTIME_IMAGES {
        let (reference, digest) = pinned.split_once('@').expect("runtime images are pinned");
        if !out
            .iter()
            .any(|e| e.reference == reference && e.digest == digest)
        {
            out.push(Entry {
                reference: reference.to_string(),
                digest: digest.to_string(),
                kind: Kind::Image,
            });
        }
    }
    Ok(out)
}

fn run(tool: &'static str, reference: &str, cmd: &mut Command) -> Result<(), OfflineError> {
    let out = cmd
        .output()
        .map_err(|source| OfflineError::Spawn { tool, source })?;
    if out.status.success() {
        return Ok(());
    }
    Err(OfflineError::Tool {
        tool,
        reference: reference.to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

fn is_runtime(e: &Entry) -> bool {
    RUNTIME_IMAGES
        .iter()
        .any(|p| p.split_once('@') == Some((e.reference.as_str(), e.digest.as_str())))
}

/// Copy a runtime image's index and only its Linux platforms into the layout, tagged by the
/// index digest as every other entry is. `skopeo` loads the host's platform from it under the
/// index digest; the missing Windows children are never read.
fn pack_linux_only(e: &Entry, layout: &Path) -> Result<(), OfflineError> {
    let pinned = format!("{}@{}", e.reference, e.digest);
    let out = Command::new("oras")
        .args(["manifest", "fetch", &pinned])
        .output()
        .map_err(|source| OfflineError::Spawn {
            tool: "oras",
            source,
        })?;
    let tool_err = |stderr: String| OfflineError::Tool {
        tool: "oras",
        reference: pinned.clone(),
        stderr,
    };
    if !out.status.success() {
        return Err(tool_err(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let bytes = out.stdout;
    let got = crate::pkg::sha256(&bytes);
    if got != e.digest {
        return Err(tool_err(format!("the manifest fetched hashes to {got}")));
    }
    let index: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|err| tool_err(format!("not JSON: {err}")))?;
    let Some(children) = index["manifests"].as_array() else {
        // A single-platform manifest: nothing to leave out.
        return run(
            "oras",
            &pinned,
            Command::new("oras")
                .args(["copy", "--to-oci-layout", &pinned])
                .arg(format!("{}:{}", layout.display(), e.tag())),
        );
    };
    for child in children.iter().filter(|c| c["platform"]["os"] == "linux") {
        let Some(digest) = child["digest"].as_str() else {
            continue;
        };
        run(
            "oras",
            &pinned,
            Command::new("oras")
                .args(["copy", "--to-oci-layout"])
                .arg(format!("{}@{digest}", e.reference))
                .arg(format!("{}:{}", layout.display(), digest.replace(':', "-"))),
        )?;
    }
    let hex = e.digest.trim_start_matches("sha256:");
    let blob = layout.join("blobs/sha256").join(hex);
    std::fs::create_dir_all(blob.parent().expect("a blob has a folder"))
        .map_err(io("creating", &blob))?;
    std::fs::write(&blob, &bytes).map_err(io("writing", &blob))?;
    let index_path = layout.join("index.json");
    let mut layout_index: serde_json::Value = match std::fs::read(&index_path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|err| OfflineError::NotALayout {
            layout: layout.to_path_buf(),
            detail: format!("index.json: {err}"),
        })?,
        Err(_) => serde_json::json!({ "schemaVersion": 2, "manifests": [] }),
    };
    let media_type = index["mediaType"]
        .as_str()
        .unwrap_or("application/vnd.oci.image.index.v1+json");
    let manifests =
        layout_index["manifests"]
            .as_array_mut()
            .ok_or_else(|| OfflineError::NotALayout {
                layout: layout.to_path_buf(),
                detail: "index.json has no `manifests`".into(),
            })?;
    manifests.push(serde_json::json!({
        "mediaType": media_type,
        "digest": e.digest,
        "size": bytes.len(),
        "annotations": { "org.opencontainers.image.ref.name": e.tag() },
    }));
    let json = serde_json::to_vec_pretty(&layout_index).expect("an index always serializes");
    std::fs::write(&index_path, json).map_err(io("writing", &index_path))
}

/// Copy everything `lock` references into the layout at `layout`, then write its index. Refuses a
/// reference pinned by tag only before copying anything.
pub fn pack(lock: &Lock, layout: &Path) -> Result<Vec<Entry>, OfflineError> {
    let entries = entries(lock)?;
    std::fs::create_dir_all(layout).map_err(io("creating", layout))?;
    for e in &entries {
        if is_runtime(e) {
            pack_linux_only(e, layout)?;
            continue;
        }
        let from = format!("{}@{}", e.reference, e.digest);
        run(
            "oras",
            &from,
            Command::new("oras")
                .args(["copy", "--to-oci-layout", &from])
                .arg(format!("{}:{}", layout.display(), e.tag())),
        )?;
    }
    let index = layout.join(INDEX_FILE);
    let json = serde_json::to_vec_pretty(&entries).expect("entries always serialize");
    std::fs::write(&index, json).map_err(io("writing", &index))?;
    Ok(entries)
}

/// What [`load`] did with one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loaded {
    Loaded,
    AlreadyHere,
}

/// The entries of a layout, read from its index.
pub fn read_index(layout: &Path) -> Result<Vec<Entry>, OfflineError> {
    let index = layout.join(INDEX_FILE);
    let text = std::fs::read_to_string(&index).map_err(|e| OfflineError::NotALayout {
        layout: layout.to_path_buf(),
        detail: format!("no `{INDEX_FILE}` ({e})"),
    })?;
    serde_json::from_str(&text).map_err(|e| OfflineError::NotALayout {
        layout: layout.to_path_buf(),
        detail: format!("`{INDEX_FILE}`: {e}"),
    })
}

/// Load a layout's parts into `cache_dir` and its images into the Podman store, skipping what is
/// already there. A rootless host runs `skopeo` inside `podman unshare`, as Podman's own store
/// needs the user namespace.
pub fn load(layout: &Path, cache_dir: &Path) -> Result<Vec<(Entry, Loaded)>, OfflineError> {
    let entries = read_index(layout)?;
    let mut out = Vec::new();
    for e in entries {
        let source = format!("{}:{}", layout.display(), e.tag());
        let state = match e.kind {
            Kind::Part => {
                let dest = crate::registry::cache_path(cache_dir, &e.digest).map_err(|err| {
                    OfflineError::NotALayout {
                        layout: layout.to_path_buf(),
                        detail: err.to_string(),
                    }
                })?;
                if dest.is_dir() {
                    Loaded::AlreadyHere
                } else {
                    let partial = cache_dir.join(format!(".{}.partial", e.tag()));
                    let _ = std::fs::remove_dir_all(&partial);
                    std::fs::create_dir_all(&partial).map_err(io("creating", &partial))?;
                    let pulled = run(
                        "oras",
                        &e.reference,
                        Command::new("oras")
                            .args(["pull", "--oci-layout", &source, "-o"])
                            .arg(&partial),
                    );
                    if let Err(err) = pulled {
                        let _ = std::fs::remove_dir_all(&partial);
                        return Err(err);
                    }
                    std::fs::rename(&partial, &dest).map_err(io("moving into the cache", &dest))?;
                    Loaded::Loaded
                }
            }
            Kind::Image => {
                let pinned = format!("{}@{}", e.reference, e.digest);
                let present = Command::new("podman")
                    .args(["image", "exists", &pinned])
                    .status()
                    .is_ok_and(|s| s.success());
                if present {
                    Loaded::AlreadyHere
                } else {
                    let copy = [
                        "copy".to_string(),
                        "--preserve-digests".to_string(),
                        format!("oci:{source}"),
                        format!("containers-storage:{pinned}"),
                    ];
                    let mut cmd = if nix::unistd::geteuid().is_root() {
                        Command::new("skopeo")
                    } else {
                        let mut c = Command::new("podman");
                        c.args(["unshare", "skopeo"]);
                        c
                    };
                    run("skopeo", &pinned, cmd.args(copy))?;
                    Loaded::Loaded
                }
            }
        };
        out.push((e, state));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(reference: &str, digest: Option<&str>) -> LockedRef {
        LockedRef {
            reference: reference.into(),
            digest: digest.map(str::to_string),
        }
    }

    fn lock(parts: Vec<LockedRef>, images: Vec<LockedRef>) -> Lock {
        let mut l = Lock::from_yaml(
            "schema: vxp-lock/1\npackage: { id: acme/demo, version: 0.1.0 }\nlocked_with: x\nfiles: {}\n",
        )
        .unwrap();
        l.parts = parts;
        l.images = images;
        l
    }

    #[test]
    fn everything_published_travels_and_a_tag_only_ref_is_refused() {
        let d = "sha256:aa";
        let got = entries(&lock(
            vec![r("ghcr.io/vxedge/parts/zone/b", Some(d))],
            vec![r("ghcr.io/vxedge/parts/agent/p", Some(d))],
        ))
        .unwrap();
        // The lock's two, then the infra image every container node needs, which no lock lists.
        assert_eq!(got.len(), 3);
        assert_eq!((got[0].kind, got[1].kind), (Kind::Part, Kind::Image));
        assert_eq!(got[0].tag(), "sha256-aa");
        assert_eq!(
            format!("{}@{}", got[2].reference, got[2].digest),
            crate::containers::spec::PAUSE_IMAGE
        );
        // A package that names the infra image itself carries it once.
        let (pause, digest) = crate::containers::spec::PAUSE_IMAGE
            .split_once('@')
            .unwrap();
        let once = entries(&lock(vec![], vec![r(pause, Some(digest))])).unwrap();
        assert_eq!(once.len(), 1);
        let refused = entries(&lock(vec![], vec![r("docker.io/library/busybox", None)]));
        assert!(
            matches!(&refused, Err(OfflineError::Unpinned(v)) if v == &["docker.io/library/busybox"])
        );
        assert_eq!(
            layout_path(Path::new("/x/demo-0.1.0.vxp")),
            PathBuf::from("/x/demo-0.1.0.parts")
        );
    }

    #[test]
    fn a_layout_without_an_index_is_not_one() {
        let dir = std::env::temp_dir().join(format!("vxoffline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            read_index(&dir),
            Err(OfflineError::NotALayout { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}
