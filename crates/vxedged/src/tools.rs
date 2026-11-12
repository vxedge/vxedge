//! The provenance of the tool binaries vxedged runs: what each `--*-bin` flag resolves to, the
//! version it reports, the sha256 of the file and its build profile.
//!
//! `TelemetryService.Collect` writes the list to the ingest folder's `tools.json`, which
//! vxtelemetry records in the archive's `metadata.json` under `tools`. Every configured tool is
//! listed, whether or not the run used it, with the daemon itself first.
//!
//! A tool is hashed and asked for `--version` once per file: the result is kept against the
//! file's device, inode, size and modification time, so a binary rebuilt while the daemon runs is
//! read again at the next collect, and an unchanged one costs a `stat`.
//!
//! `profile` is `debug` or `release` only when the tool says so: vxedged knows its own, and
//! another tool's `--version` line may end in `(debug)` or `(release)`. Anything else is
//! `unknown`; it is never guessed from the path.

use serde::Serialize;
use sha2::Digest;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::Mutex;

/// The file in the ingest folder vxtelemetry records as `metadata.json`'s `tools`.
pub const FILE: &str = "tools.json";

/// How long a tool gets to answer `--version`.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// One tool, as `tools.json` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tool {
    /// The tool, as its flag names it: `vxprobe` for `--vxprobe-bin`.
    pub name: String,
    /// The file that runs, symlinks resolved; the configured value when it does not resolve.
    pub path: String,
    /// The version its `--version` reports, empty when it reports none.
    pub version: String,
    /// The sha256 of the file, hex; empty when it cannot be read.
    pub sha256: String,
    /// `debug`, `release` or `unknown`.
    pub profile: String,
    /// Why the tool could not be resolved, read or run, when it could not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What a file is, for the cache: the same key means the same bytes.
type FileKey = (u64, u64, u64, i64, i64);

fn file_key(m: &std::fs::Metadata) -> FileKey {
    (m.dev(), m.ino(), m.size(), m.mtime(), m.mtime_nsec())
}

/// The configured tools, and what each was found to be.
pub struct Tools {
    /// `(name, configured value)`, in the order they are listed.
    configured: Vec<(String, String)>,
    /// The daemon's own entry, read at the first list.
    own: tokio::sync::OnceCell<Tool>,
    cache: Mutex<HashMap<PathBuf, (FileKey, Tool)>>,
}

impl Tools {
    /// `configured` is `(name, flag value)` per tool.
    pub fn new(configured: Vec<(String, String)>) -> Self {
        Tools {
            configured,
            own: tokio::sync::OnceCell::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Every configured tool, the daemon first. Reads only the files that changed since the
    /// last call.
    pub async fn list(&self) -> Vec<Tool> {
        let own = self
            .own
            .get_or_init(|| async {
                tokio::task::spawn_blocking(own)
                    .await
                    .expect("reading the daemon's own file does not panic")
            })
            .await;
        let mut out = vec![own.clone()];
        let mut cache = self.cache.lock().await;
        for (name, configured) in &self.configured {
            out.push(read_tool(name, configured, &mut cache).await);
        }
        out
    }

    /// Write [`Tools::list`] to `dir`'s `tools.json`.
    pub async fn write(&self, dir: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.list().await).expect("tools always serialize");
        std::fs::write(dir.join(FILE), bytes)
    }
}

/// The daemon itself: its own version and profile, and the file it was started from.
fn own() -> Tool {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let exe = std::env::current_exe();
    // `/proc/self/exe` is the running file even after it is replaced on disk.
    let (sha256, error) = match sha256_of(Path::new("/proc/self/exe")) {
        Ok(s) => (s, None),
        Err(e) => (
            String::new(),
            Some(format!("cannot read the daemon's own file: {e}")),
        ),
    };
    Tool {
        name: "vxedged".into(),
        path: exe
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        version: env!("CARGO_PKG_VERSION").into(),
        sha256,
        profile: profile.into(),
        error,
    }
}

async fn read_tool(
    name: &str,
    configured: &str,
    cache: &mut HashMap<PathBuf, (FileKey, Tool)>,
) -> Tool {
    let failed = |error: String| Tool {
        name: name.into(),
        path: configured.into(),
        version: String::new(),
        sha256: String::new(),
        profile: "unknown".into(),
        error: Some(error),
    };
    let Some(found) = resolve(configured, std::env::var_os("PATH").as_deref()) else {
        return failed(format!("`{configured}` is not a file, nor on PATH"));
    };
    let path = found.canonicalize().unwrap_or(found);
    let key = match std::fs::metadata(&path) {
        Ok(m) => file_key(&m),
        Err(e) => return failed(format!("cannot stat `{}`: {e}", path.display())),
    };
    if let Some((k, tool)) = cache.get(&path)
        && *k == key
        && tool.name == name
    {
        return tool.clone();
    }
    let mut tool = Tool {
        name: name.into(),
        path: path.to_string_lossy().into_owned(),
        version: String::new(),
        sha256: String::new(),
        profile: "unknown".into(),
        error: None,
    };
    let file = path.clone();
    let digest = tokio::task::spawn_blocking(move || sha256_of(&file))
        .await
        .expect("hashing a file does not panic");
    match digest {
        Ok(s) => tool.sha256 = s,
        Err(e) => tool.error = Some(format!("cannot read `{}`: {e}", path.display())),
    }
    match version_line(&path).await {
        Ok(line) => (tool.version, tool.profile) = parse_version(&line),
        Err(e) => {
            tool.error.get_or_insert(e);
        }
    }
    cache.insert(path, (key, tool.clone()));
    tool
}

/// The file `configured` runs as, the way `Command::new` finds it: a value with a `/` is a path,
/// anything else is looked up on `path_var`.
fn resolve(configured: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if configured.is_empty() {
        return None;
    }
    if configured.contains('/') {
        let p = PathBuf::from(configured);
        return p.is_file().then_some(p);
    }
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(configured))
        .find(|p| {
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

fn sha256_of(path: &Path) -> std::io::Result<String> {
    Ok(crate::services::hex::encode(sha2::Sha256::digest(
        std::fs::read(path)?,
    )))
}

/// The first line `path --version` prints.
async fn version_line(path: &Path) -> Result<String, String> {
    let run = tokio::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(VERSION_TIMEOUT, run)
        .await
        .map_err(|_| format!("`{} --version` did not answer", path.display()))?
        .map_err(|e| format!("cannot run `{} --version`: {e}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "`{} --version` failed ({})",
            path.display(),
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string())
}

/// `(version, profile)` from a `--version` line such as `vxprobe 0.2.2 (release)`: the word after
/// the name, and the profile when the line ends in `(debug)` or `(release)`.
fn parse_version(line: &str) -> (String, String) {
    let words: Vec<&str> = line.split_whitespace().collect();
    let profile = match words.last() {
        Some(&"(debug)") => "debug",
        Some(&"(release)") => "release",
        _ => "unknown",
    };
    let version = match words.as_slice() {
        [] => String::new(),
        [only] => only.to_string(),
        [_, v, ..] if !v.starts_with('(') => v.to_string(),
        [first, ..] => first.to_string(),
    };
    (version, profile.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn the_version_and_profile_come_from_the_version_line() {
        let p = parse_version;
        assert_eq!(p("vxprobe 0.2.2"), ("0.2.2".into(), "unknown".into()));
        assert_eq!(
            p("vxprobe 0.2.2 (release)"),
            ("0.2.2".into(), "release".into())
        );
        assert_eq!(p("vxota 0.3.0 (debug)"), ("0.3.0".into(), "debug".into()));
        assert_eq!(p("1.4"), ("1.4".into(), "unknown".into()));
        assert_eq!(p(""), (String::new(), "unknown".into()));
        // A profile word anywhere but last says nothing.
        assert_eq!(
            p("vxprobe (release) 0.2.2"),
            ("vxprobe".into(), "unknown".into())
        );
    }

    #[test]
    fn a_bare_name_is_looked_up_on_path_and_a_path_is_taken_as_given() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let tool = script(&bin, "vxprobe", "true");
        std::fs::write(bin.join("vxtraffic"), "not executable").unwrap();
        let path = std::env::join_paths([dir.path(), &bin]).unwrap();
        assert_eq!(resolve("vxprobe", Some(&path)), Some(tool.clone()));
        assert_eq!(resolve("vxtraffic", Some(&path)), None);
        assert_eq!(resolve("vxprobe", None), None);
        assert_eq!(resolve(tool.to_str().unwrap(), None), Some(tool));
        assert_eq!(resolve(bin.join("nope").to_str().unwrap(), None), None);
        assert_eq!(resolve("", Some(&path)), None);
    }

    #[tokio::test]
    async fn each_tool_is_listed_with_its_file_digest_version_and_profile() {
        let dir = tempfile::tempdir().unwrap();
        let probe = script(dir.path(), "vxprobe", "echo 'vxprobe 0.2.2 (release)'");
        let ota = script(dir.path(), "vxota", "echo 'vxota 0.3.0'");
        let broken = script(dir.path(), "vxtraffic", "exit 2");
        let tools = Tools::new(vec![
            ("vxprobe".into(), probe.to_string_lossy().into()),
            ("vxota".into(), ota.to_string_lossy().into()),
            ("vxtraffic".into(), broken.to_string_lossy().into()),
            ("vxtelemetry".into(), "vxedge-no-such-tool".into()),
        ]);
        let list = tools.list().await;
        let names: Vec<&str> = list.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            ["vxedged", "vxprobe", "vxota", "vxtraffic", "vxtelemetry"]
        );

        let own = &list[0];
        assert_eq!(own.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            own.profile,
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
        assert_eq!(own.sha256.len(), 64);

        let p = &list[1];
        assert_eq!(p.path, probe.canonicalize().unwrap().to_string_lossy());
        assert_eq!(
            (p.version.as_str(), p.profile.as_str()),
            ("0.2.2", "release")
        );
        assert_eq!(p.sha256, sha256_of(&probe).unwrap());
        assert_eq!(p.error, None);

        assert_eq!(
            (list[2].version.as_str(), list[2].profile.as_str()),
            ("0.3.0", "unknown")
        );

        // A tool that runs but fails `--version` keeps its digest and says why it has no version.
        let b = &list[3];
        assert_eq!(b.sha256, sha256_of(&broken).unwrap());
        assert_eq!((b.version.as_str(), b.profile.as_str()), ("", "unknown"));
        assert!(b.error.as_deref().unwrap().contains("failed"), "{b:?}");

        // A tool that is not there is still listed, as configured.
        let missing = &list[4];
        assert_eq!(missing.path, "vxedge-no-such-tool");
        assert_eq!(missing.sha256, "");
        assert!(missing.error.is_some());
    }

    #[tokio::test]
    async fn a_tool_is_read_once_until_its_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let body = |v: &str| format!("echo x >> '{}'\necho 'vxprobe {v}'", count.display());
        let probe = script(dir.path(), "vxprobe", &body("0.2.2"));
        let tools = Tools::new(vec![("vxprobe".into(), probe.to_string_lossy().into())]);
        let runs = || std::fs::read_to_string(&count).unwrap().lines().count();

        let first = tools.list().await;
        let again = tools.list().await;
        assert_eq!(first, again);
        assert_eq!(runs(), 1, "an unchanged file is not run again");

        // Rebuilt in place: a new size, so read again.
        std::fs::write(&probe, format!("#!/bin/sh\n{}\n", body("0.2.3-rebuilt"))).unwrap();
        let rebuilt = tools.list().await;
        assert_eq!(runs(), 2);
        assert_eq!(rebuilt[1].version, "0.2.3-rebuilt");
        assert_ne!(rebuilt[1].sha256, first[1].sha256);
    }

    #[tokio::test]
    async fn the_list_is_written_as_tools_json() {
        let dir = tempfile::tempdir().unwrap();
        let probe = script(dir.path(), "vxprobe", "echo 'vxprobe 0.2.2 (debug)'");
        let tools = Tools::new(vec![("vxprobe".into(), probe.to_string_lossy().into())]);
        tools.write(dir.path()).await.unwrap();
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(FILE)).unwrap()).unwrap();
        let entry = &written[1];
        assert_eq!(entry["name"], "vxprobe");
        assert_eq!(entry["version"], "0.2.2");
        assert_eq!(entry["profile"], "debug");
        assert_eq!(entry["sha256"].as_str().unwrap().len(), 64);
        assert!(entry.get("error").is_none(), "no error, no key");
        let mut keys: Vec<&String> = entry.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["name", "path", "profile", "sha256", "version"]);
    }
}
