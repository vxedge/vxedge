//! **The daemon's own exporter** (`ARCHITECTURE.md` §12): what the host spends on each node and
//! what each slice's queue holds, served as Prometheus text on a loopback port and scraped by the
//! workspace's telemetry service like any other target.
//!
//! The daemon serves these because it already knows which process or cgroup runs each node and
//! which HTB class carries each slice, and it maps both to entity paths once, the same way it
//! writes `entities.json`. Every series carries `entity="<path>"`, so one target serves many
//! entities. Nothing is cached: each scrape reads the kernel's counters as they are then.
//!
//! - `vxedge_node_cpu_seconds_total`, `vxedge_node_memory_bytes`: a node's container cgroup, or
//!   its VMM or runner process (`source` says which). A node nothing on this host runs (a bare
//!   namespace, an attached platform, a node that is down) has no series.
//! - `vxedge_slice_*`: the counters of the slice's HTB class on the gateway uplink. The class is
//!   the gateway's, so every vehicle whose links use the slice reports the same queue.
//! - `vxedge_process_*{process="vxedged"}` on `host`: the observer's own cost.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

use vxedge_core::entities::{self, HOST};
use vxedge_core::gateway::{GatewayPlan, Shell};
use vxedge_core::model::Experiment;
use vxedge_core::zones::usage::{self, Source};

use crate::services::SharedState;

/// The address the exporter is bound to, once it is.
static BOUND: OnceLock<String> = OnceLock::new();

/// The scrape targets this daemon adds to `host`: its exporter, when one is listening.
pub fn targets() -> Vec<String> {
    BOUND.get().cloned().into_iter().collect()
}

/// Bind `listen` (`host:port`; port 0 picks one). A failure is logged and the daemon runs
/// without an exporter: its experiments then have no host-side series, and nothing else changes.
pub async fn bind(listen: &str) -> Option<TcpListener> {
    if listen.is_empty() || listen == "off" {
        return None;
    }
    match TcpListener::bind(listen).await {
        Ok(l) => {
            let addr = l.local_addr().ok()?.to_string();
            let _ = BOUND.set(addr);
            Some(l)
        }
        Err(e) => {
            warn!(%listen, error = %e, "metrics exporter not started: cannot bind");
            None
        }
    }
}

/// Serve `/metrics` until the daemon exits.
pub async fn serve(listener: TcpListener, state: SharedState, shell: Shell, runtime_dir: PathBuf) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let (state, shell, runtime_dir) = (state.clone(), shell.clone(), runtime_dir.clone());
        tokio::spawn(async move {
            if let Err(e) = answer(stream, state, shell, runtime_dir).await {
                debug!(error = %e, "metrics request");
            }
        });
    }
}

async fn answer(
    mut stream: TcpStream,
    state: SharedState,
    shell: Shell,
    runtime_dir: PathBuf,
) -> std::io::Result<()> {
    let mut head = Vec::with_capacity(512);
    let mut buf = [0u8; 512];
    let read = async {
        while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 8192 {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        Ok::<_, std::io::Error>(())
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .map_err(|_| std::io::Error::other("request timed out"))??;
    let line = String::from_utf8_lossy(&head);
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next(), parts.next());
    let (status, body) = if method == Some("GET") && path == Some("/metrics") {
        let snapshot = {
            let s = state.lock().await;
            match (&s.active, &s.gateway) {
                (Some(a), gw) => Some((a.model.clone(), a.dir.clone(), gw.clone())),
                (None, _) => None,
            }
        };
        let body = tokio::task::spawn_blocking(move || render(snapshot, &shell, &runtime_dir))
            .await
            .unwrap_or_default();
        ("200 OK", body)
    } else {
        ("404 Not Found", "only GET /metrics is served\n".to_string())
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; version=0.0.4\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// One metric family.
struct Family {
    name: &'static str,
    help: &'static str,
    kind: &'static str,
    samples: Vec<(Vec<(&'static str, String)>, f64)>,
}

impl Family {
    fn new(name: &'static str, kind: &'static str, help: &'static str) -> Self {
        Self {
            name,
            help,
            kind,
            samples: Vec::new(),
        }
    }
}

/// The page for one scrape. With nothing active, only the observer's own cost.
fn render(
    active: Option<(Experiment, PathBuf, Option<GatewayPlan>)>,
    sh: &Shell,
    runtime_dir: &std::path::Path,
) -> String {
    let mut cpu = Family::new(
        "vxedge_node_cpu_seconds_total",
        "counter",
        "CPU time the host spent running the node, user and system.",
    );
    let mut mem = Family::new(
        "vxedge_node_memory_bytes",
        "gauge",
        "Memory the node holds on the host: its cgroup's memory.current, or its process's RSS.",
    );
    let mut slice = [
        (
            Family::new(
                "vxedge_slice_sent_bytes_total",
                "counter",
                "Bytes the slice's HTB class sent on the uplink.",
            ),
            (|c: &vxedge_core::gateway::stats::ClassStats| c.bytes) as fn(&_) -> u64,
        ),
        (
            Family::new(
                "vxedge_slice_sent_packets_total",
                "counter",
                "Packets the slice's HTB class sent on the uplink.",
            ),
            |c| c.packets,
        ),
        (
            Family::new(
                "vxedge_slice_dropped_packets_total",
                "counter",
                "Packets the slice's queue dropped.",
            ),
            |c| c.drops,
        ),
        (
            Family::new(
                "vxedge_slice_overlimits_total",
                "counter",
                "Times the slice's class was over its rate and held a packet back.",
            ),
            |c| c.overlimits,
        ),
        (
            Family::new(
                "vxedge_slice_backlog_bytes",
                "gauge",
                "Bytes queued in the slice's class now.",
            ),
            |c| c.backlog,
        ),
        (
            Family::new(
                "vxedge_slice_backlog_packets",
                "gauge",
                "Packets queued in the slice's class now.",
            ),
            |c| c.qlen,
        ),
    ];
    let mut own_cpu = Family::new(
        "vxedge_process_cpu_seconds_total",
        "counter",
        "CPU time of an observer process on the host.",
    );
    let mut own_mem = Family::new(
        "vxedge_process_memory_bytes",
        "gauge",
        "Resident memory of an observer process on the host.",
    );
    if let Some(u) = usage::read(&Source::Process(std::process::id() as i32)) {
        let labels = vec![("entity", HOST.to_string()), ("process", "vxedged".into())];
        own_cpu.samples.push((labels.clone(), u.cpu_seconds));
        own_mem.samples.push((labels, u.memory_bytes as f64));
    }

    if let Some((model, dir, Some(gateway))) = &active {
        let mut seen = std::collections::BTreeSet::new();
        let nodes =
            vxedge_core::zones::plan_nodes_from_model(model, gateway, dir).unwrap_or_default();
        for np in nodes.iter().filter(|n| seen.insert(n.node_id.clone())) {
            let (Some(path), Some(source)) = (
                entities::node_path(model, &np.node_id),
                vxedge_core::zones::node_usage_source(sh, np, runtime_dir),
            ) else {
                continue;
            };
            let Some(u) = usage::read(&source) else {
                continue;
            };
            let labels = vec![("entity", path), ("source", source.kind().to_string())];
            cpu.samples.push((labels.clone(), u.cpu_seconds));
            mem.samples.push((labels, u.memory_bytes as f64));
        }
        let classes =
            vxedge_core::gateway::stats::class_stats(sh, &gateway.uplink).unwrap_or_default();
        for s in &gateway.slices {
            let handle = format!("1:{:x}", s.class_minor);
            let Some(c) = classes.iter().find(|c| c.handle == handle) else {
                continue;
            };
            for vehicle in &model.entities {
                if !vehicle
                    .links
                    .iter()
                    .any(|l| l.slice.as_deref() == Some(&s.id))
                {
                    continue;
                }
                let labels = vec![("entity", format!("{}/slices/{}", vehicle.id, s.id))];
                for (family, value) in &mut slice {
                    family.samples.push((labels.clone(), value(c) as f64));
                }
            }
        }
    }

    let mut families = vec![cpu, mem];
    families.extend(slice.into_iter().map(|(f, _)| f));
    families.extend([own_cpu, own_mem]);
    text(&families)
}

/// Prometheus text exposition (0.0.4). A family with no samples is left out.
fn text(families: &[Family]) -> String {
    let mut out = String::new();
    for f in families.iter().filter(|f| !f.samples.is_empty()) {
        let _ = writeln!(out, "# HELP {} {}", f.name, f.help);
        let _ = writeln!(out, "# TYPE {} {}", f.name, f.kind);
        for (labels, value) in &f.samples {
            let block: Vec<String> = labels
                .iter()
                .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
                .collect();
            let _ = writeln!(out, "{}{{{}}} {value}", f.name, block.join(","));
        }
    }
    out
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_render_as_prometheus_text_with_entity_labels() {
        let mut f = Family::new("vxedge_node_memory_bytes", "gauge", "Memory.");
        f.samples.push((
            vec![
                ("entity", "car/front/ecu1".into()),
                ("source", "cgroup".into()),
            ],
            512000.0,
        ));
        let empty = Family::new("vxedge_slice_backlog_bytes", "gauge", "Unused.");
        assert_eq!(
            text(&[f, empty]),
            "# HELP vxedge_node_memory_bytes Memory.\n# TYPE vxedge_node_memory_bytes gauge\n\
             vxedge_node_memory_bytes{entity=\"car/front/ecu1\",source=\"cgroup\"} 512000\n"
        );
        assert_eq!(escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn with_nothing_active_only_the_observer_is_reported() {
        let sh = Shell {
            netns: None,
            escalate: false,
        };
        let page = render(None, &sh, std::path::Path::new("/nonexistent"));
        assert!(
            page.contains("vxedge_process_cpu_seconds_total{entity=\"host\",process=\"vxedged\"}")
        );
        assert!(!page.contains("vxedge_node_"));
    }
}
