//! Running a `checks` block's steps, in order, during a trial's measure phase.
//!
//! Each tool step is a container outside any group, labelled [`LABEL`] with its step id, in the
//! network namespace its step names: a container node's group (`container:<infra>`), a
//! background step's container, or none of its own (`alone`, loopback only). Its output is
//! written line by line to `<out>/<step>.log` as it comes, which the telemetry ingest reads into
//! the trial's records under the entity `checks/<step>`. After each step's own output the runner
//! adds one line of its own, `{"op": "vx.step", …}`: the exit code, whether it timed out, how many
//! attempts it took, so a case can tell a step that ran from one that did not.
//!
//! The steps run within a budget, the trial's measure duration. A step is cut short by the time
//! left, and once the budget is spent the remaining steps are skipped: their cases are then
//! `inconclusive`, never passed.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::{Value as Json, json};
use tokio::sync::Notify;

use super::{ALONE, Checks, Observe, STEP_OP, STEPS_MOUNT, Step, Tool, line, matches};
use crate::containers::ContainerClient;
use crate::containers::spec::{Bind, ContainerSpec};

/// The label every check container carries, valued with its step id.
pub const LABEL: &str = "org.vxedge.check";
/// Seconds a tool gets after SIGTERM before it is killed.
const STOP_TIMEOUT_S: u32 = 2;
/// How long a step's output may take to drain after its container exits.
const DRAIN: Duration = Duration::from_secs(5);

/// What the runner needs from the host.
pub struct Context<'a> {
    pub client: &'a ContainerClient,
    /// The package folder, which `files` are read from.
    pub root: &'a Path,
    /// Where each step's output is written: `<runtime>/checks`.
    pub out: &'a Path,
    /// The longest the steps may take together.
    pub budget: Duration,
    /// The slot of the run the steps act on: a step `on` a node runs in that node's container
    /// group, which carries the slot.
    pub instance: crate::instance::Instance,
}

/// How a run went.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub ran: usize,
    /// Steps not started because the budget was spent.
    pub skipped: usize,
}

/// The file step `id`'s output is written to.
pub fn output_file(out: &Path, id: &str) -> PathBuf {
    out.join(format!("{id}.log"))
}

/// The container a step's tool runs in, in the run in slot `instance`: two runs may run the same
/// package's checks at once.
pub fn container_name(instance: crate::instance::Instance, id: &str) -> String {
    format!("vx{instance}-check-{id}")
}

/// What one step's container printed, as it prints it.
#[derive(Clone, Default)]
struct Output {
    lines: Arc<Mutex<Vec<String>>>,
    more: Arc<Notify>,
}

impl Output {
    fn snapshot(&self) -> Vec<String> {
        self.lines.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

fn append(file: &Path, text: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
    {
        let _ = writeln!(f, "{text}");
    }
}

/// Copy a container's output into `file` line by line, and into `output`, until it ends.
fn follow(
    client: &ContainerClient,
    container: &str,
    file: PathBuf,
    output: Output,
) -> tokio::task::JoinHandle<()> {
    let mut stream = client.follow_output(container);
    tokio::spawn(async move {
        let mut pending: Vec<u8> = Vec::new();
        let emit = |raw: &[u8]| {
            let text = String::from_utf8_lossy(raw.strip_suffix(b"\r").unwrap_or(raw)).into_owned();
            append(&file, &text);
            if let Ok(mut l) = output.lines.lock() {
                l.push(text);
            }
            output.more.notify_waiters();
        };
        while let Some(Ok(chunk)) = stream.next().await {
            pending.extend_from_slice(&chunk);
            while let Some(i) = pending.iter().position(|b| *b == b'\n') {
                let rest = pending.split_off(i + 1);
                emit(&pending[..i]);
                pending = rest;
            }
        }
        if !pending.is_empty() {
            emit(&pending);
        }
        output.more.notify_waiters();
    })
}

/// A background step still running.
struct Background {
    container: String,
    file: PathBuf,
    follower: tokio::task::JoinHandle<()>,
    started: Instant,
    ready: Option<bool>,
}

struct Runner<'a> {
    checks: &'a Checks,
    ctx: &'a Context<'a>,
    deadline: Instant,
    background: BTreeMap<String, Background>,
}

fn secs(d: Duration) -> f64 {
    (d.as_secs_f64() * 1000.0).round() / 1000.0
}

impl Runner<'_> {
    fn left(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// `want` seconds, cut to the budget left.
    fn capped(&self, want: f64) -> Duration {
        Duration::from_secs_f64(want.max(0.0)).min(self.left())
    }

    async fn network(&self, step: &Step) -> Result<String, String> {
        if let Some(w) = &step.with {
            return self
                .background
                .get(w)
                .map(|b| format!("container:{}", b.container))
                .ok_or_else(|| format!("background step `{w}` is not running"));
        }
        match step.on.as_deref() {
            Some(ALONE) | None => Ok("none".into()),
            Some(node) => {
                let group = crate::zones::plan::runtime_name_of(self.ctx.instance, node);
                self.ctx
                    .client
                    .handle(&group)
                    .await
                    .map(|h| format!("container:{}", h.infra_id))
                    .map_err(|e| format!("node `{node}` has no container group up: {e}"))
            }
        }
    }

    fn spec(&self, id: &str, tool: &Tool, step: &Step) -> ContainerSpec {
        let mut binds: Vec<Bind> = step
            .files
            .iter()
            .map(|(inside, file)| Bind {
                host: self.ctx.root.join(file),
                container: inside.clone(),
                read_only: true,
                relabel: false,
            })
            .collect();
        binds.push(Bind {
            host: self.ctx.out.to_path_buf(),
            container: STEPS_MOUNT.to_string(),
            read_only: true,
            relabel: false,
        });
        ContainerSpec {
            name: id.to_string(),
            image: tool.image.clone(),
            cmd: tool.args.iter().chain(&step.args).cloned().collect(),
            entrypoint: tool.entrypoint.clone().unwrap_or_default(),
            env: tool.env.clone(),
            binds,
            stop_timeout_s: Some(STOP_TIMEOUT_S),
            ..Default::default()
        }
    }

    async fn start(
        &self,
        id: &str,
        step: &Step,
    ) -> Result<(String, Output, tokio::task::JoinHandle<()>), String> {
        let tool = step
            .tool
            .as_ref()
            .and_then(|t| self.checks.tools.get(t))
            .ok_or_else(|| format!("step `{id}` names no declared tool"))?;
        let network = self.network(step).await?;
        let name = container_name(self.ctx.instance, id);
        let labels = BTreeMap::from([(LABEL.to_string(), id.to_string())]);
        let container = self
            .ctx
            .client
            .start_standalone(&name, &self.spec(id, tool, step), &network, labels)
            .await
            .map_err(|e| e.to_string())?;
        let output = Output::default();
        let follower = follow(
            self.ctx.client,
            &container,
            output_file(self.ctx.out, id),
            output.clone(),
        );
        Ok((container, output, follower))
    }

    async fn finish(&self, container: &str, follower: tokio::task::JoinHandle<()>) {
        let _ = tokio::time::timeout(DRAIN, follower).await;
        let _ = self.ctx.client.remove_container(container).await;
    }

    /// Run a tool in the foreground, again `until` a line matches when it says so.
    async fn foreground(&self, id: &str, step: &Step) -> Json {
        let file = output_file(self.ctx.out, id);
        let attempts = if step.until.is_some() {
            step.attempts.unwrap_or(1)
        } else {
            1
        };
        let started = Instant::now();
        let mut record = json!({ "op": STEP_OP });
        for attempt in 1..=attempts {
            let (container, output, follower) = match self.start(id, step).await {
                Ok(s) => s,
                Err(e) => {
                    record["error"] = e.into();
                    break;
                }
            };
            let exit = tokio::time::timeout(
                self.capped(step.timeout_s()),
                self.ctx.client.wait_exit(&container),
            )
            .await;
            let timed_out = exit.is_err();
            if timed_out {
                let _ = self.ctx.client.remove_container(&container).await;
            }
            self.finish(&container, follower).await;
            record["exit"] = exit
                .ok()
                .and_then(Result::ok)
                .map_or(Json::Null, Json::from);
            record["timed_out"] = timed_out.into();
            record["attempts"] = attempt.into();
            let Some(until) = &step.until else { break };
            let matched = output.snapshot().iter().any(|l| matches(&line(l), until));
            record["matched"] = matched.into();
            if matched || attempt == attempts || self.left().is_zero() {
                break;
            }
            tokio::time::sleep(self.capped(step.every_s.unwrap_or(0.5))).await;
        }
        record["elapsed_s"] = secs(started.elapsed()).into();
        append(&file, &record.to_string());
        record
    }

    async fn background_start(&mut self, id: &str, step: &Step) {
        let file = output_file(self.ctx.out, id);
        let (container, output, follower) = match self.start(id, step).await {
            Ok(s) => s,
            Err(e) => {
                append(&file, &json!({ "op": STEP_OP, "error": e }).to_string());
                return;
            }
        };
        let started = Instant::now();
        let mut ready = None;
        if let Some(want) = &step.ready {
            let wait = self.capped(step.timeout_s());
            let seen = tokio::time::timeout(wait, async {
                loop {
                    let more = output.more.notified();
                    if output.snapshot().iter().any(|l| matches(&line(l), want)) {
                        return true;
                    }
                    if follower.is_finished() {
                        // It exited without the line; nothing more will come.
                        return false;
                    }
                    more.await;
                }
            })
            .await;
            ready = Some(matches!(seen, Ok(true)));
        }
        self.background.insert(
            id.to_string(),
            Background {
                container,
                file,
                follower,
                started,
                ready,
            },
        );
    }

    /// End a background step: wait for it to exit, or stop it, and record how it ended.
    async fn background_end(&mut self, id: &str, wait: Option<f64>) {
        let Some(b) = self.background.remove(id) else {
            return;
        };
        let mut record = json!({ "op": STEP_OP });
        match wait {
            Some(t) => {
                let exit =
                    tokio::time::timeout(self.capped(t), self.ctx.client.wait_exit(&b.container))
                        .await;
                record["timed_out"] = exit.is_err().into();
                record["exit"] = exit
                    .ok()
                    .and_then(Result::ok)
                    .map_or(Json::Null, Json::from);
                if record["timed_out"] == true {
                    let _ = self.ctx.client.remove_container(&b.container).await;
                }
            }
            None => {
                // Whether it was still running when stopped: a server that exited on its own is
                // not one that served until the end.
                let running = self
                    .ctx
                    .client
                    .state(&b.container)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|s| s.running);
                let _ = self.ctx.client.remove_container(&b.container).await;
                record["stopped"] = true.into();
                record["running_at_stop"] = running.into();
            }
        }
        if let Some(r) = b.ready {
            record["ready"] = r.into();
        }
        self.finish(&b.container, b.follower).await;
        record["elapsed_s"] = secs(b.started.elapsed()).into();
        append(&b.file, &record.to_string());
    }

    async fn observe(&self, id: &str, step: &Step) {
        let node = step.on.as_deref().unwrap_or_default();
        let record = match step.observe {
            Some(Observe::Member) => {
                let member = step.member.as_deref().unwrap_or_default();
                let name = format!(
                    "{}-{member}",
                    crate::zones::plan::runtime_name_of(self.ctx.instance, node)
                );
                match self.ctx.client.state(&name).await {
                    Ok(Some(s)) => json!({
                        "op": "member", "member": member, "exists": true, "running": s.running,
                        "restarts": s.restarts, "started_at": s.started_at,
                    }),
                    Ok(None) => json!({ "op": "member", "member": member, "exists": false }),
                    Err(e) => json!({ "op": STEP_OP, "error": e.to_string() }),
                }
            }
            Some(Observe::Port) => {
                let port = crate::gateway::ifnames::access_port(self.ctx.instance, node, None);
                let stats = Path::new("/sys/class/net").join(&port).join("statistics");
                let read = |f: &str| {
                    std::fs::read_to_string(stats.join(f))
                        .ok()
                        .and_then(|s| s.trim().parse::<u64>().ok())
                };
                match read("rx_packets") {
                    Some(rx) => json!({
                        "op": "port", "port": port, "exists": true, "rx_packets": rx,
                        "tx_packets": read("tx_packets"), "rx_bytes": read("rx_bytes"),
                        "tx_bytes": read("tx_bytes"),
                    }),
                    None => json!({ "op": "port", "port": port, "exists": false }),
                }
            }
            None => return,
        };
        append(&output_file(self.ctx.out, id), &record.to_string());
    }
}

/// Run `checks`' steps in order within `ctx.budget`. Every tool image is made present first, so
/// no step's time includes a download. Containers a crashed run left are removed before, and
/// every container this run started is removed after, whatever happened.
pub async fn run(checks: &Checks, ctx: &Context<'_>) -> Result<Summary, String> {
    std::fs::create_dir_all(ctx.out)
        .map_err(|e| format!("cannot create `{}`: {e}", ctx.out.display()))?;
    for id in checks.recorded() {
        let _ = std::fs::remove_file(output_file(ctx.out, id));
    }
    remove_leftovers(ctx.client).await;
    for (id, tool) in &checks.tools {
        ctx.client
            .ensure_image(&tool.image)
            .await
            .map_err(|e| format!("tool `{id}`: {e}"))?;
    }
    let mut r = Runner {
        checks,
        ctx,
        deadline: Instant::now() + ctx.budget,
        background: BTreeMap::new(),
    };
    let mut summary = Summary::default();
    for (i, step) in checks.steps.iter().enumerate() {
        if r.left().is_zero() {
            summary.skipped = checks.steps.len() - i;
            break;
        }
        summary.ran += 1;
        let id = step.id.as_deref().unwrap_or_default();
        if step.tool.is_some() {
            if step.background {
                r.background_start(id, step).await;
            } else {
                r.foreground(id, step).await;
            }
        } else if step.observe.is_some() {
            r.observe(id, step).await;
        } else if let Some(w) = &step.wait {
            r.background_end(w, Some(step.timeout_s())).await;
        } else if let Some(s) = &step.stop {
            r.background_end(s, None).await;
        } else if let Some(s) = step.sleep_s {
            tokio::time::sleep(r.capped(s)).await;
        }
    }
    let left: Vec<String> = r.background.keys().cloned().collect();
    for id in left {
        r.background_end(&id, None).await;
    }
    remove_leftovers(ctx.client).await;
    Ok(summary)
}

/// Remove every check container, of this run or of one that crashed.
pub async fn remove_leftovers(client: &ContainerClient) {
    for id in client.labelled(LABEL).await.unwrap_or_default() {
        let _ = client.remove_container(&id).await;
    }
}
