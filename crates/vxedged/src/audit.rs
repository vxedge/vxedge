//! Structured audit log: every mutating vxAPI call is recorded as one JSON
//! line in the runtime dir (best effort — a read-only dir degrades to
//! tracing-only) and always emitted as a tracing event with target "audit".

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

pub struct Audit {
    file: Option<Mutex<std::fs::File>>,
}

impl Audit {
    pub fn new(runtime_dir: &Path) -> Self {
        let path = runtime_dir.join("audit.jsonl");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path);
        match file {
            Ok(f) => Self {
                file: Some(Mutex::new(f)),
            },
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                    "audit file unavailable; auditing to tracing only");
                Self { file: None }
            }
        }
    }

    pub fn record(&self, rpc: &str, detail: serde_json::Value) {
        tracing::info!(target: "audit", rpc = %rpc, detail = %detail, "mutating call");
        if let Some(file) = &self.file {
            let line = serde_json::json!({
                "ts_unix_ms": crate::jobs::now_ms(),
                "rpc": rpc,
                "detail": detail,
            });
            if let Ok(mut f) = file.lock() {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}
