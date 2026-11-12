//! `ResultService`: the archives this host holds, described from each archive's own
//! `metadata.json` and nothing else, and fetched by digest (`ARCHITECTURE.md` §13). An archive is
//! found from a campaign's record (`campaign_run`), or in a workspace's `results/` folder.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use sha2::Digest;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use vxedge_api::v1::result_service_server::ResultService;
use vxedge_api::v1::{
    ResultChunk, ResultCompareRequest, ResultComparison, ResultFetchRequest, ResultGetRequest,
    ResultInfo, ResultList, ResultListRequest, ResultTrial, ResultVerdict, ResultVerdictRequest,
};

use crate::errors::{self, R};

/// The most an archive is read in one chunk of `Fetch`.
const CHUNK: usize = 1 << 20;

pub struct ResultSvc {
    pub runtime_dir: PathBuf,
    /// Archives `List` found in a workspace's `results/`, by digest, so `Get` and `Fetch` reach
    /// them too. Each is hashed again before it is served.
    pub seen: std::sync::Mutex<std::collections::HashMap<String, PathBuf>>,
}

fn sha256_file(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(format!(
        "sha256:{}",
        crate::services::hex::encode(h.finalize())
    ))
}

/// The JSON value as a typed parameter value: integers stay integers.
fn param(v: &serde_json::Value) -> vxedge_api::v1::ParamValue {
    use vxedge_api::v1::param_value::Value as V;
    let value = match v {
        serde_json::Value::Bool(b) => V::Boolean(*b),
        serde_json::Value::Number(n) if n.is_i64() => V::Integer(n.as_i64().unwrap_or_default()),
        serde_json::Value::Number(n) => V::Number(n.as_f64().unwrap_or_default()),
        serde_json::Value::String(s) => V::Text(s.clone()),
        other => V::Text(other.to_string()),
    };
    vxedge_api::v1::ParamValue { value: Some(value) }
}

/// An archive's `ResultInfo`, from its bytes' digest and its `metadata.json`. `None` for a file
/// that is not a results archive.
pub(crate) fn describe(path: &Path, digest: Option<String>) -> Option<ResultInfo> {
    let file = std::fs::File::open(path).ok()?;
    let size_bytes = file.metadata().ok()?.len();
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut text = String::new();
    zip.by_name("metadata.json")
        .ok()?
        .read_to_string(&mut text)
        .ok()?;
    let m: serde_json::Value = serde_json::from_str(&text).ok()?;
    let s = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
    let package = &m["package"];
    let trial = m
        .get("trial")
        .filter(|t| t.is_object())
        .map(|t| ResultTrial {
            campaign_id: s(&t["campaign_id"]),
            trial_id: s(&t["trial_id"]),
            arm: s(&t["arm"]),
            repetition: t["repetition"].as_u64().unwrap_or_default() as u32,
            parameters: t["parameters"]
                .as_object()
                .map(|o| o.iter().map(|(k, v)| (k.clone(), param(v))).collect())
                .unwrap_or_default(),
        });
    Some(ResultInfo {
        digest: digest.or_else(|| sha256_file(path))?,
        path: path.display().to_string(),
        size_bytes,
        archive: m["archive"].as_str().unwrap_or("vxarchive/1").to_string(),
        experiment_id: s(&m["experiment_id"]),
        package_id: s(&package["id"]),
        package_version: s(&package["version"]),
        package_digest: s(&package["digest"]),
        package_locked: package["locked"].as_bool(),
        trial,
        collected_unix_ms: m["collected_at_unix_ms"].as_i64().unwrap_or_default(),
        vxtelemetry_version: s(&m["vxtelemetry_version"]),
        kernel_release: s(&m["kernel"]["release"]),
    })
}

/// Every archive the host's campaigns wrote, optionally one campaign's only.
fn from_campaigns(runtime_dir: &Path, campaign_id: &str) -> Vec<ResultInfo> {
    crate::campaign_run::all(runtime_dir)
        .into_iter()
        .filter(|c| campaign_id.is_empty() || c.campaign_id == campaign_id)
        .flat_map(|c| c.trials)
        .filter(|t| !t.archive.is_empty())
        .filter_map(|t| {
            let digest = (!t.sha256.is_empty()).then(|| format!("sha256:{}", t.sha256));
            describe(Path::new(&t.archive), digest)
        })
        .collect()
}

/// Every archive under `root`'s `results/`.
fn from_folder(root: &Path) -> Vec<ResultInfo> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "zip") {
                out.push(p);
            }
        }
    }
    let mut paths = Vec::new();
    walk(&root.join("results"), &mut paths);
    paths.iter().filter_map(|p| describe(p, None)).collect()
}

fn listed(runtime_dir: &Path, campaign_id: &str, root: Option<&Path>) -> Vec<ResultInfo> {
    let mut out = from_campaigns(runtime_dir, campaign_id);
    if let Some(root) = root {
        for r in from_folder(root) {
            let in_campaign = campaign_id.is_empty()
                || r.trial
                    .as_ref()
                    .is_some_and(|t| t.campaign_id == campaign_id);
            if in_campaign && !out.iter().any(|o| o.digest == r.digest) {
                out.push(r);
            }
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.collected_unix_ms));
    out
}

fn find(runtime_dir: &Path, seen: Option<PathBuf>, digest: &str) -> Result<ResultInfo, Status> {
    listed(runtime_dir, "", None)
        .into_iter()
        .find(|r| r.digest == digest)
        .or_else(|| {
            seen.and_then(|p| describe(&p, None))
                .filter(|r| r.digest == digest)
        })
        .ok_or_else(|| {
            errors::not_found(
                R::ResultUnknown,
                format!("no archive `{digest}` among this host's results (`ResultService.List`)"),
            )
        })
}

/// The folder of a workspace named by `m`: a directory, a file's directory, or a cached package.
/// Only the folder matters here, not its documents: a results folder is read as it is. `None` when
/// `m` names no folder.
fn folder_of(m: Option<&vxedge_api::v1::ManifestRef>) -> Result<Option<PathBuf>, Status> {
    use vxedge_api::v1::manifest_ref::Source;
    Ok(match m.and_then(|m| m.source.as_ref()) {
        Some(Source::Path(p)) => {
            let p = PathBuf::from(p);
            Some(if p.is_dir() {
                p
            } else {
                p.parent().map(PathBuf::from).unwrap_or(p)
            })
        }
        Some(Source::Package(digest)) => Some(crate::package::cached_dir(digest)?),
        _ => None,
    })
}

/// The campaign recorded in `root`'s `results/<id>/campaign.json`, for a campaign this host did
/// not run. Each archive it names is read from that folder: a relative path below it, or an
/// absolute one by its file name, as when the record was written on another host. Each must hash
/// to the record's `sha256`, so the reference is the one that was published.
pub(crate) fn recorded_in(
    root: &Path,
    id: &str,
) -> Result<crate::campaign_run::CampaignRecord, Status> {
    use std::path::Component;
    let one_name = |p: &Path| {
        let mut parts = p.components();
        matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(_)), None)
        )
    };
    if !one_name(Path::new(id)) {
        return Err(errors::not_found(
            R::CampaignUnknown,
            format!("`{id}` names no campaign: it must be one folder name under `results/`"),
        ));
    }
    let folder = root.join("results").join(id);
    let file = folder.join("campaign.json");
    let bytes = std::fs::read(&file).map_err(|_| {
        errors::not_found(
            R::CampaignUnknown,
            format!("no campaign record `{}`", file.display()),
        )
    })?;
    let mut rec: crate::campaign_run::CampaignRecord =
        serde_json::from_slice(&bytes).map_err(|e| {
            errors::precondition(
                R::ResultUnknown,
                format!("`{}` is not a campaign record: {e}", file.display()),
            )
        })?;
    if rec.campaign_id != id {
        return Err(errors::precondition(
            R::ResultUnknown,
            format!(
                "`{}` records campaign `{}`, not `{id}`",
                file.display(),
                rec.campaign_id
            ),
        ));
    }
    for t in rec
        .trials
        .iter_mut()
        .filter(|t| t.state == "succeeded" && !t.archive.is_empty())
    {
        let named = Path::new(&t.archive);
        let below = named
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        let path = if named.is_relative() && below {
            folder.join(named)
        } else if let Some(name) = named.is_absolute().then(|| named.file_name()).flatten() {
            folder.join(name)
        } else {
            return Err(errors::precondition(
                R::ResultUnknown,
                format!(
                    "`{}`, trial `{}`: archive `{}` is outside the record's folder",
                    file.display(),
                    t.id,
                    t.archive
                ),
            ));
        };
        let want = t.sha256.strip_prefix("sha256:").unwrap_or(&t.sha256);
        let got = sha256_file(&path).ok_or_else(|| {
            errors::not_found(
                R::ResultUnknown,
                format!("trial `{}`: no archive `{}`", t.id, path.display()),
            )
        })?;
        if want.is_empty() {
            return Err(errors::precondition(
                R::ChecksumMismatch,
                format!(
                    "`{}`, trial `{}`: the record names no sha256",
                    file.display(),
                    t.id
                ),
            ));
        }
        if got.strip_prefix("sha256:") != Some(want) {
            return Err(errors::precondition(
                R::ChecksumMismatch,
                format!(
                    "trial `{}`: `{}` is {got}, the record says `{}`",
                    t.id,
                    path.display(),
                    t.sha256
                ),
            ));
        }
        t.archive = path.display().to_string();
    }
    Ok(rec)
}

#[tonic::async_trait]
impl ResultService for ResultSvc {
    type FetchStream = Pin<Box<dyn Stream<Item = Result<ResultChunk, Status>> + Send>>;

    async fn list(&self, req: Request<ResultListRequest>) -> Result<Response<ResultList>, Status> {
        let req = req.into_inner();
        let root = folder_of(req.workspace.as_ref())?;
        let runtime_dir = self.runtime_dir.clone();
        let results = tokio::task::spawn_blocking(move || {
            listed(&runtime_dir, &req.campaign_id, root.as_deref())
        })
        .await
        .map_err(|e| Status::internal(format!("list task panicked: {e}")))?;
        if let Ok(mut seen) = self.seen.lock() {
            for r in &results {
                seen.insert(r.digest.clone(), PathBuf::from(&r.path));
            }
        }
        Ok(Response::new(ResultList { results }))
    }

    async fn get(&self, req: Request<ResultGetRequest>) -> Result<Response<ResultInfo>, Status> {
        let digest = req.into_inner().digest;
        let runtime_dir = self.runtime_dir.clone();
        let seen = self.seen.lock().ok().and_then(|m| m.get(&digest).cloned());
        let info = tokio::task::spawn_blocking(move || find(&runtime_dir, seen, &digest))
            .await
            .map_err(|e| Status::internal(format!("get task panicked: {e}")))??;
        Ok(Response::new(info))
    }

    async fn verdict(
        &self,
        req: Request<ResultVerdictRequest>,
    ) -> Result<Response<ResultVerdict>, Status> {
        let req = req.into_inner();
        let id = req.campaign_id;
        let rec = crate::campaign_run::read(&self.runtime_dir, &id)
            .ok_or_else(|| crate::campaign::unknown(&id))?;
        let root = folder_of(req.reference_workspace.as_ref())?;
        if req.reference_workspace.is_some() && root.is_none() {
            return Err(errors::invalid(
                R::WorkspaceInvalid,
                "`reference_workspace` names a folder: a path or a package digest, not inline YAML",
            ));
        }
        let runtime_dir = self.runtime_dir.clone();
        let judged = tokio::task::spawn_blocking(move || {
            let lookup = crate::verdict::Lookup {
                runtime_dir: &runtime_dir,
                workspace: root.as_deref(),
            };
            crate::verdict::of_campaign(&rec, &lookup).map(|v| crate::verdict::to_proto(&rec, &v))
        })
        .await
        .map_err(|e| Status::internal(format!("verdict task panicked: {e}")))?;
        judged
            .map(Response::new)
            .map_err(|e| errors::precondition(R::ResultUnknown, e))
    }

    async fn compare(
        &self,
        req: Request<ResultCompareRequest>,
    ) -> Result<Response<ResultComparison>, Status> {
        let req = req.into_inner();
        let rec = crate::campaign_run::read(&self.runtime_dir, &req.campaign_id)
            .ok_or_else(|| crate::campaign::unknown(&req.campaign_id))?;
        let root = folder_of(req.reference_workspace.as_ref())?;
        if req.reference_workspace.is_some() && root.is_none() {
            return Err(errors::invalid(
                R::WorkspaceInvalid,
                "`reference_workspace` names a folder: a path or a package digest, not inline YAML",
            ));
        }
        let runtime_dir = self.runtime_dir.clone();
        let compared = tokio::task::spawn_blocking(move || {
            let id = &req.reference_campaign_id;
            let reference = match &root {
                Some(root) => recorded_in(root, id)?,
                None => crate::campaign_run::read(&runtime_dir, id)
                    .ok_or_else(|| crate::campaign::unknown(id))?,
            };
            let lookup = crate::verdict::Lookup {
                runtime_dir: &runtime_dir,
                workspace: root.as_deref(),
            };
            crate::verdict::compare(&rec, &reference, &lookup)
                .map(|c| crate::verdict::comparison_to_proto(&rec, &reference, &c))
                .map_err(|e| errors::precondition(R::ResultUnknown, e))
        })
        .await
        .map_err(|e| Status::internal(format!("compare task panicked: {e}")))?;
        compared.map(Response::new)
    }

    async fn fetch(
        &self,
        req: Request<ResultFetchRequest>,
    ) -> Result<Response<Self::FetchStream>, Status> {
        let digest = req.into_inner().digest;
        let runtime_dir = self.runtime_dir.clone();
        let (d, rt) = (digest.clone(), runtime_dir.clone());
        let seen = self.seen.lock().ok().and_then(|m| m.get(&digest).cloned());
        let info = tokio::task::spawn_blocking(move || find(&rt, seen, &d))
            .await
            .map_err(|e| Status::internal(format!("fetch task panicked: {e}")))??;
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tokio::task::spawn_blocking(move || {
            let send = |m| tx.blocking_send(m).is_ok();
            let Ok(mut f) = std::fs::File::open(&info.path) else {
                let _ = send(Err(Status::internal(format!(
                    "cannot read `{}`",
                    info.path
                ))));
                return;
            };
            let mut buf = vec![0u8; CHUNK];
            let mut first = true;
            loop {
                let n = match f.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(e) => {
                        let _ = send(Err(Status::internal(e.to_string())));
                        return;
                    }
                };
                let chunk = ResultChunk {
                    digest: if first {
                        info.digest.clone()
                    } else {
                        String::new()
                    },
                    size_bytes: if first { info.size_bytes } else { 0 },
                    data: buf[..n].to_vec(),
                };
                first = false;
                if !send(Ok(chunk)) {
                    return;
                }
            }
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }
}
