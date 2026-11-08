//! Async job engine: every mutating vxAPI RPC submits work here and returns a
//! `Job` immediately; progress is observable via `JobService.Watch` (backed by
//! a `tokio::sync::watch` channel per job) and on the event bus.
//!
//! Three rules an agent relies on live here rather than in each service:
//! - **One lifecycle operation at a time.** A second call for the same thing returns the running
//!   job, and any other is refused as busy. Stages of a composite job are internal calls and do
//!   not take the claim, or a composite would block itself.
//! - **Retries are safe.** A `request_id` returns the job it first started.
//! - **Ids never repeat.** Each daemon gets its own prefix, and history is bounded, so an id from
//!   before a restart, or one that aged out, is unknown rather than another job.

use sha2::Digest;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, watch};
use tonic::{Code, Request, Status};
use vxedge_api::v1::{ErrorReason, Job, JobState};

use crate::errors::{self, R};
use crate::events::EventBus;

/// Terminal jobs kept for `Get`, `Watch` and `List`. Running jobs are never dropped.
const HISTORY: usize = 512;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as i64
}

pub fn is_terminal(state: i32) -> bool {
    state == JobState::Succeeded as i32
        || state == JobState::Failed as i32
        || state == JobState::Cancelled as i32
}

/// Why a job did not succeed: its reason, its message, or that it was cancelled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFailure {
    pub reason: ErrorReason,
    pub message: String,
    pub cancelled: bool,
}

impl JobFailure {
    pub fn new(reason: ErrorReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
            cancelled: false,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            reason: ErrorReason::Unspecified,
            message: "cancelled".to_string(),
            cancelled: true,
        }
    }

    /// The same failure, with what it happened in named first. The reason is kept.
    pub fn context(mut self, prefix: impl std::fmt::Display) -> Self {
        self.message = format!("{prefix}: {}", self.message);
        self
    }
}

impl JobFailure {
    /// A refusal from a stage's RPC, keeping its reason.
    pub fn from_status(status: &Status) -> Self {
        use tonic_types::StatusExt;
        let reason = status
            .get_error_details()
            .error_info()
            .and_then(|i| ErrorReason::from_str_name(&i.reason))
            .unwrap_or(ErrorReason::Unspecified);
        Self::new(reason, status.message())
    }

    /// Several failures as one: the first reason that says something, and every message.
    pub fn joined(prefix: impl std::fmt::Display, failures: Vec<JobFailure>) -> Self {
        let reason = failures
            .iter()
            .map(|f| f.reason)
            .find(|r| *r != ErrorReason::Unspecified)
            .unwrap_or(ErrorReason::Unspecified);
        let messages: Vec<String> = failures.into_iter().map(|f| f.message).collect();
        Self::new(reason, format!("{prefix}: {}", messages.join("; ")))
    }
}

impl std::fmt::Display for JobFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for JobFailure {
    fn from(message: String) -> Self {
        Self::new(ErrorReason::Unspecified, message)
    }
}

impl From<&str> for JobFailure {
    fn from(message: &str) -> Self {
        Self::new(ErrorReason::Unspecified, message)
    }
}

/// Marks a request as a stage of a composite job: it names its parent, and it does not take the
/// lifecycle claim the parent already holds.
#[derive(Clone, Debug)]
pub struct Stage {
    pub parent: String,
}

/// A request a composite job makes to another service.
pub fn stage<T>(message: T, parent: &str) -> Request<T> {
    let mut req = Request::new(message);
    req.extensions_mut().insert(Stage {
        parent: parent.to_string(),
    });
    req
}

/// How a job is submitted: its parent, its lifecycle claim and its request id.
#[derive(Clone, Debug, Default)]
pub struct JobOpts {
    parent: String,
    claim: Option<String>,
    request_key: Option<String>,
}

impl JobOpts {
    /// A lifecycle verb on `target`. A stage of a composite job takes no claim.
    pub fn lifecycle<T>(req: &Request<T>, target: impl Into<String>) -> Self {
        match req.extensions().get::<Stage>() {
            Some(stage) => Self {
                parent: stage.parent.clone(),
                ..Default::default()
            },
            None => Self {
                claim: Some(target.into()),
                ..Default::default()
            },
        }
    }

    /// A verb that does not converge, so a retry with the same id returns the original job.
    pub fn request<T>(req: &Request<T>, request_id: &str) -> Self {
        Self {
            parent: req
                .extensions()
                .get::<Stage>()
                .map(|s| s.parent.clone())
                .unwrap_or_default(),
            claim: None,
            request_key: (!request_id.is_empty()).then(|| request_id.to_string()),
        }
    }
}

/// Handle passed to job bodies for reporting progress.
pub struct Progress {
    id: String,
    tx: watch::Sender<Job>,
    cancel: Arc<AtomicBool>,
}

impl Progress {
    pub fn update(&self, percent: u32, message: &str) {
        self.tx.send_modify(|job| {
            job.state = JobState::Running as i32;
            job.progress_percent = percent;
            job.message = message.to_string();
            job.updated_unix_ms = now_ms();
        });
    }

    /// What this job produced, reported with its result.
    pub fn artifact(&self, path: &std::path::Path, sha256: &str) {
        self.tx.send_modify(|job| {
            job.artifact = Some(vxedge_api::v1::Artifact {
                path: path.display().to_string(),
                sha256: sha256.to_string(),
            });
        });
    }

    /// This job's id, for the stages it starts.
    pub fn job_id(&self) -> &str {
        &self.id
    }

    /// Where a job may stop if it was cancelled: between steps, never inside one.
    pub fn checkpoint(&self) -> Result<(), JobFailure> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(JobFailure::cancelled())
        } else {
            Ok(())
        }
    }
}

struct Entry {
    seq: u64,
    tx: watch::Sender<Job>,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    jobs: HashMap<String, Entry>,
    /// `(kind, target, job id)` of the lifecycle job running now.
    lifecycle: Option<(String, String, String)>,
    /// `kind:request_id` to the job it started.
    requests: HashMap<String, String>,
}

#[derive(Clone)]
pub struct JobEngine {
    inner: Arc<Inner>,
}

struct Inner {
    seq: AtomicU64,
    boot: String,
    state: Mutex<State>,
    events: EventBus,
    runtime_dir: Option<PathBuf>,
}

/// A short id for this daemon's lifetime, so a job id from before a restart cannot match.
pub(crate) fn boot_id() -> String {
    static ENGINES: AtomicU64 = AtomicU64::new(0);
    let n = ENGINES.fetch_add(1, Ordering::Relaxed);
    let seed = format!("{}-{}-{n}", std::process::id(), now_ms());
    crate::services::hex::encode(sha2::Sha256::digest(seed.as_bytes()))[..6].to_string()
}

/// Filters for [`JobEngine::list`]. Empty means any.
#[derive(Default)]
pub struct JobFilter {
    pub states: Vec<i32>,
    pub kind: String,
    pub run_id: String,
    pub parent: String,
}

impl JobEngine {
    /// The event bus this engine publishes job transitions on.
    ///
    /// Exposed so a service can emit a *domain fact* on the same bus without carrying its own
    /// handle: every service already holds a `JobEngine`, and a second field for the bus would
    /// be two ways to reach one thing. Job progress stays private — that belongs to the job's
    /// watch channel, not to a shared bus.
    pub fn events(&self) -> &EventBus {
        &self.inner.events
    }

    /// The target of the lifecycle job running now (`vehicle <id>`, `experiment <id> [stages]`,
    /// `zone <vxid>`, …), if one is.
    pub async fn lifecycle_target(&self) -> Option<String> {
        let st = self.inner.state.lock().await;
        st.lifecycle.as_ref().map(|(_, target, _)| target.clone())
    }

    #[cfg(test)]
    pub fn new(events: EventBus) -> Self {
        Self::build(events, None)
    }

    /// An engine that stamps each job with the run active in `runtime_dir`.
    pub fn with_runtime_dir(events: EventBus, runtime_dir: PathBuf) -> Self {
        Self::build(events, Some(runtime_dir))
    }

    fn build(events: EventBus, runtime_dir: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                seq: AtomicU64::new(0),
                boot: boot_id(),
                state: Mutex::new(State::default()),
                events,
                runtime_dir,
            }),
        }
    }

    fn run_id(inner: &Inner) -> String {
        inner
            .runtime_dir
            .as_deref()
            .and_then(crate::services::active_run)
            .map(|r| r.run_id().to_string())
            .unwrap_or_default()
    }

    /// Submit a job with no claim, parent or request id. `work` receives a `Progress` handle; its
    /// `Ok(message)` / `Err(error)` decides the terminal state. Returns the initial snapshot.
    pub async fn submit<F, Fut, E>(&self, kind: &str, work: F) -> Job
    where
        F: FnOnce(Progress) -> Fut + Send + 'static,
        Fut: Future<Output = Result<String, E>> + Send + 'static,
        E: Into<JobFailure> + Send + 'static,
    {
        self.submit_with(JobOpts::default(), kind, work)
            .await
            .expect("a job with no claim or request id is never refused")
    }

    /// Submit a job under `opts`. A lifecycle claim held by another job refuses this one as busy,
    /// unless it is the same kind on the same target, which returns the running job. A request id
    /// already seen returns the job it started.
    pub async fn submit_with<F, Fut, E>(
        &self,
        opts: JobOpts,
        kind: &str,
        work: F,
    ) -> Result<Job, Status>
    where
        F: FnOnce(Progress) -> Fut + Send + 'static,
        Fut: Future<Output = Result<String, E>> + Send + 'static,
        E: Into<JobFailure> + Send + 'static,
    {
        let inner = self.inner.clone();
        let mut st = inner.state.lock().await;
        let request_key = opts.request_key.map(|r| format!("{kind}:{r}"));
        if let Some(key) = &request_key
            && let Some(entry) = st.requests.get(key).and_then(|id| st.jobs.get(id))
        {
            return Ok(entry.tx.borrow().clone());
        }
        if let Some(target) = &opts.claim
            && let Some((held_kind, held_target, held_id)) = &st.lifecycle
        {
            if held_kind == kind
                && held_target == target
                && let Some(entry) = st.jobs.get(held_id)
            {
                return Ok(entry.tx.borrow().clone());
            }
            return Err(errors::refuse(
                Code::Aborted,
                R::Busy,
                format!(
                    "`{held_kind}` on `{held_target}` is running ({held_id}): one lifecycle \
                     operation runs at a time"
                ),
            ));
        }

        let seq = inner.seq.fetch_add(1, Ordering::Relaxed);
        let id = format!("job-{}-{seq:06x}", inner.boot);
        let job = Job {
            id: id.clone(),
            kind: kind.to_string(),
            state: JobState::Pending as i32,
            created_unix_ms: now_ms(),
            updated_unix_ms: now_ms(),
            parent_job_id: opts.parent,
            run_id: Self::run_id(&inner),
            ..Default::default()
        };
        let (tx, _) = watch::channel(job.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        st.jobs.insert(
            id.clone(),
            Entry {
                seq,
                tx: tx.clone(),
                cancel: cancel.clone(),
            },
        );
        if let Some(target) = opts.claim {
            st.lifecycle = Some((kind.to_string(), target, id.clone()));
        }
        if let Some(key) = request_key {
            st.requests.insert(key, id.clone());
        }
        prune(&mut st);
        drop(st);

        let events = inner.events.clone();
        let job_kind = kind.to_string();
        events.emit(crate::events::EventKind::JobState, &id, "pending");
        tokio::spawn(async move {
            tx.send_modify(|j| {
                j.state = JobState::Running as i32;
                j.updated_unix_ms = now_ms();
            });
            events.emit(crate::events::EventKind::JobState, &id, "running");

            let progress = Progress {
                id: id.clone(),
                tx: tx.clone(),
                cancel,
            };
            let result = work(progress).await.map_err(Into::into);
            let run_id = Self::run_id(&inner);

            match result {
                Ok(message) => {
                    tx.send_modify(|j| {
                        j.state = JobState::Succeeded as i32;
                        j.progress_percent = 100;
                        j.message = message;
                        j.updated_unix_ms = now_ms();
                        if j.run_id.is_empty() {
                            j.run_id = run_id;
                        }
                    });
                    events.emit(crate::events::EventKind::JobState, &id, "succeeded");
                    tracing::info!(job = %id, kind = %job_kind, "job succeeded");
                }
                Err(failure) => {
                    let (state, word) = if failure.cancelled {
                        (JobState::Cancelled, "cancelled")
                    } else {
                        (JobState::Failed, "failed")
                    };
                    tx.send_modify(|j| {
                        j.state = state as i32;
                        j.error = failure.message.clone();
                        j.error_reason = failure.reason as i32;
                        j.updated_unix_ms = now_ms();
                        if j.run_id.is_empty() {
                            j.run_id = run_id;
                        }
                    });
                    events.emit(crate::events::EventKind::JobState, &id, word);
                    tracing::warn!(job = %id, kind = %job_kind, error = %failure, "job {word}");
                }
            }
            let mut st = inner.state.lock().await;
            if st
                .lifecycle
                .as_ref()
                .is_some_and(|(_, _, held)| *held == id)
            {
                st.lifecycle = None;
            }
        });

        Ok(job)
    }

    pub async fn get(&self, id: &str) -> Option<Job> {
        self.inner
            .state
            .lock()
            .await
            .jobs
            .get(id)
            .map(|e| e.tx.borrow().clone())
    }

    pub async fn watch(&self, id: &str) -> Option<watch::Receiver<Job>> {
        self.inner
            .state
            .lock()
            .await
            .jobs
            .get(id)
            .map(|e| e.tx.subscribe())
    }

    /// Jobs matching `filter`, most recent first, from `offset`, at most `limit`, and whether
    /// more remain.
    pub async fn list(&self, filter: &JobFilter, offset: usize, limit: usize) -> (Vec<Job>, bool) {
        let st = self.inner.state.lock().await;
        let mut jobs: Vec<(u64, Job)> = st
            .jobs
            .values()
            .map(|e| (e.seq, e.tx.borrow().clone()))
            .filter(|(_, j)| filter.states.is_empty() || filter.states.contains(&j.state))
            .filter(|(_, j)| filter.kind.is_empty() || j.kind == filter.kind)
            .filter(|(_, j)| filter.run_id.is_empty() || j.run_id == filter.run_id)
            .filter(|(_, j)| filter.parent.is_empty() || j.parent_job_id == filter.parent)
            .collect();
        jobs.sort_by_key(|(seq, _)| std::cmp::Reverse(*seq));
        let more = jobs.len() > offset + limit;
        let page = jobs
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(_, j)| j)
            .collect();
        (page, more)
    }

    /// Ask a job, and the stages it started, to stop at their next checkpoint.
    /// Whether a lifecycle operation holds the host now.
    pub async fn lifecycle_running(&self) -> bool {
        self.inner.state.lock().await.lifecycle.is_some()
    }

    pub async fn cancel(&self, id: &str) -> Result<Job, Status> {
        let st = self.inner.state.lock().await;
        let entry = st.jobs.get(id).ok_or_else(|| unknown_job(id))?;
        let job = entry.tx.borrow().clone();
        if job.state == JobState::Cancelled as i32 {
            return Ok(job);
        }
        if is_terminal(job.state) {
            return Err(errors::precondition(
                R::NotCancellable,
                format!("job `{id}` has already finished"),
            ));
        }
        if job.kind.ends_with(".down") {
            return Err(errors::precondition(
                R::NotCancellable,
                format!("job `{id}` is a `down`: a half-finished teardown helps nobody"),
            ));
        }
        let mut pending = vec![id.to_string()];
        while let Some(next) = pending.pop() {
            if let Some(e) = st.jobs.get(&next) {
                e.cancel.store(true, Ordering::Relaxed);
            }
            pending.extend(
                st.jobs
                    .iter()
                    .filter(|(_, e)| e.tx.borrow().parent_job_id == next)
                    .map(|(k, _)| k.clone()),
            );
        }
        Ok(job)
    }
}

/// The refusal for a job id this daemon does not hold.
pub fn unknown_job(id: &str) -> Status {
    errors::not_found(
        R::JobUnknown,
        format!(
            "unknown job `{id}`: it never existed, aged out, or predates a daemon restart. Read \
             RunService for the current state"
        ),
    )
}

/// Drop the oldest finished jobs beyond the history limit, and the request ids that named them.
fn prune(st: &mut State) {
    if st.jobs.len() <= HISTORY {
        return;
    }
    let mut finished: Vec<(u64, String)> = st
        .jobs
        .iter()
        .filter(|(_, e)| is_terminal(e.tx.borrow().state))
        .map(|(id, e)| (e.seq, id.clone()))
        .collect();
    finished.sort();
    let excess = st.jobs.len() - HISTORY;
    for (_, id) in finished.into_iter().take(excess) {
        st.jobs.remove(&id);
    }
    let live: std::collections::HashSet<String> = st.jobs.keys().cloned().collect();
    st.requests.retain(|_, id| live.contains(id));
}

/// How long a member has to stay running after it starts, before the job that started it
/// succeeds. A process that rejects its arguments exits just after it starts.
pub const START_WINDOW: Duration = Duration::from_secs(2);

/// Run `start`, then require `running` to hold for all of `window`, so a member that starts and
/// exits straight away is a failure and not a success.
pub async fn start_then_confirm<S, R2, Fut>(
    start: S,
    mut running: R2,
    window: Duration,
) -> Result<(), String>
where
    S: Future<Output = Result<(), String>>,
    R2: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    start.await?;
    let deadline = tokio::time::Instant::now() + window;
    loop {
        if !running().await {
            return Err(format!(
                "started, then stopped running within {} s: see its log",
                window.as_secs()
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(());
        }
        tokio::time::sleep(window / 8).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_terminal(mut rx: watch::Receiver<Job>) -> Job {
        loop {
            let snapshot = rx.borrow().clone();
            if is_terminal(snapshot.state) {
                return snapshot;
            }
            rx.changed().await.expect("job channel closed early");
        }
    }

    fn engine() -> JobEngine {
        JobEngine::new(EventBus::new())
    }

    #[tokio::test]
    async fn job_runs_to_success_with_progress() {
        let engine = engine();
        let job = engine
            .submit("test.ok", |p| async move {
                p.update(50, "halfway");
                Ok::<_, String>("done".to_string())
            })
            .await;
        let rx = engine.watch(&job.id).await.expect("job registered");
        let terminal = wait_terminal(rx).await;
        assert_eq!(terminal.state, JobState::Succeeded as i32);
        assert_eq!(terminal.progress_percent, 100);
        assert_eq!(terminal.message, "done");
        assert!(terminal.error.is_empty());
    }

    #[tokio::test]
    async fn job_failure_carries_error_and_reason() {
        let engine = engine();
        let job = engine
            .submit("test.fail", |p| async move {
                p.update(30, "about to fail");
                Err::<String, _>(JobFailure::new(ErrorReason::NodePortGone, "port gone"))
            })
            .await;
        let terminal = wait_terminal(engine.watch(&job.id).await.unwrap()).await;
        assert_eq!(terminal.state, JobState::Failed as i32);
        assert_eq!(terminal.error, "port gone");
        assert_eq!(terminal.error_reason, ErrorReason::NodePortGone as i32);
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_job() {
        assert!(engine().get("job-nope").await.is_none());
    }

    #[tokio::test]
    async fn ids_carry_this_daemons_prefix() {
        let a = engine()
            .submit("t", |_| async { Ok::<_, String>(String::new()) })
            .await;
        let b = engine()
            .submit("t", |_| async { Ok::<_, String>(String::new()) })
            .await;
        assert_ne!(a.id, b.id, "two daemons never hand out the same id");
    }

    fn held() -> (
        tokio::sync::oneshot::Sender<()>,
        impl Future<Output = Result<String, String>>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        (tx, async move {
            let _ = rx.await;
            Ok("released".to_string())
        })
    }

    fn claim(target: &str) -> JobOpts {
        JobOpts {
            claim: Some(target.into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_second_lifecycle_call_is_busy_and_a_duplicate_gets_the_running_job() {
        let engine = engine();
        let (release, work) = held();
        let first = engine
            .submit_with(claim("car"), "vehicle.up", |_| work)
            .await
            .unwrap();
        let same = engine
            .submit_with(claim("car"), "vehicle.up", |_| async {
                Ok::<_, String>(String::new())
            })
            .await
            .unwrap();
        assert_eq!(same.id, first.id, "a duplicate gets the running job");
        let busy = engine
            .submit_with(claim("hpc1"), "zone.up", |_| async {
                Ok::<_, String>(String::new())
            })
            .await
            .unwrap_err();
        assert_eq!(busy.code(), Code::Aborted);
        use tonic_types::StatusExt;
        let info = busy.get_error_details().error_info().cloned().unwrap();
        assert_eq!(info.reason, "ERROR_REASON_BUSY");
        let staged = engine
            .submit_with(JobOpts::default(), "zone.up", |_| async {
                Ok::<_, String>(String::new())
            })
            .await;
        assert!(staged.is_ok(), "a stage takes no claim");
        release.send(()).unwrap();
        wait_terminal(engine.watch(&first.id).await.unwrap()).await;
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let next = engine
            .submit_with(claim("hpc1"), "zone.up", |_| async {
                Ok::<_, String>(String::new())
            })
            .await;
        assert!(next.is_ok(), "the claim is released when the job ends");
    }

    #[tokio::test]
    async fn a_request_id_returns_the_job_it_started() {
        let engine = engine();
        let opts = || JobOpts {
            request_key: Some("r1".into()),
            ..Default::default()
        };
        let first = engine
            .submit_with(opts(), "traffic.up", |_| async {
                Ok::<_, String>(String::new())
            })
            .await
            .unwrap();
        let again = engine
            .submit_with(opts(), "traffic.up", |_| async {
                Ok::<_, String>(String::new())
            })
            .await
            .unwrap();
        assert_eq!(again.id, first.id);
        let other = engine
            .submit_with(JobOpts::default(), "traffic.up", |_| async {
                Ok::<_, String>(String::new())
            })
            .await
            .unwrap();
        assert_ne!(other.id, first.id);
    }

    /// A composite job cancelled mid-stage stops at its next checkpoint, and so does the stage.
    #[tokio::test]
    async fn a_composite_job_cancelled_mid_stage_stops_at_the_next_checkpoint() {
        let engine = engine();
        let (go, wait) = tokio::sync::oneshot::channel::<()>();
        let child_engine = engine.clone();
        let parent = engine
            .submit("vehicle.up", move |p| async move {
                let parent_id = p.job_id().to_string();
                let child = child_engine
                    .submit_with(
                        JobOpts {
                            parent: parent_id,
                            ..Default::default()
                        },
                        "zone.up",
                        move |c| async move {
                            let _ = wait.await;
                            c.checkpoint()?;
                            Ok::<_, JobFailure>("stage done".to_string())
                        },
                    )
                    .await
                    .unwrap();
                let mut rx = child_engine.watch(&child.id).await.unwrap();
                while !is_terminal(rx.borrow().state) {
                    rx.changed().await.unwrap();
                }
                p.checkpoint()?;
                Ok::<_, JobFailure>("all stages done".to_string())
            })
            .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        engine.cancel(&parent.id).await.unwrap();
        go.send(()).unwrap();
        let terminal = wait_terminal(engine.watch(&parent.id).await.unwrap()).await;
        assert_eq!(terminal.state, JobState::Cancelled as i32);
        let filter = JobFilter {
            parent: parent.id.clone(),
            ..Default::default()
        };
        let (children, _) = engine.list(&filter, 0, 10).await;
        assert_eq!(children.len(), 1, "the stage names its parent");
        let child = wait_terminal(engine.watch(&children[0].id).await.unwrap()).await;
        assert_eq!(child.state, JobState::Cancelled as i32);
    }

    #[tokio::test]
    async fn a_down_and_a_finished_job_cannot_be_cancelled() {
        let engine = engine();
        let (release, work) = held();
        let down = engine.submit("vehicle.down", |_| work).await;
        let refused = engine.cancel(&down.id).await.unwrap_err();
        assert_eq!(refused.code(), Code::FailedPrecondition);
        release.send(()).unwrap();
        let done = engine
            .submit("vehicle.up", |_| async { Ok::<_, String>(String::new()) })
            .await;
        wait_terminal(engine.watch(&done.id).await.unwrap()).await;
        assert!(engine.cancel(&done.id).await.is_err());
        assert_eq!(
            engine.cancel("job-none").await.unwrap_err().code(),
            Code::NotFound
        );
    }

    #[tokio::test]
    async fn history_is_bounded() {
        let engine = engine();
        let mut first = String::new();
        for i in 0..HISTORY + 10 {
            let job = engine
                .submit("t", |_| async { Ok::<_, String>(String::new()) })
                .await;
            if i == 0 {
                first = job.id.clone();
            }
            wait_terminal(engine.watch(&job.id).await.unwrap()).await;
        }
        let (all, _) = engine.list(&JobFilter::default(), 0, usize::MAX).await;
        assert!(all.len() <= HISTORY + 1, "{}", all.len());
        assert!(
            engine.get(&first).await.is_none(),
            "the oldest finished job aged out"
        );
    }

    #[tokio::test]
    async fn a_member_that_exits_straight_after_starting_is_a_failure() {
        let window = Duration::from_millis(80);
        let err = start_then_confirm(async { Ok(()) }, || async { false }, window)
            .await
            .unwrap_err();
        assert!(err.contains("stopped running"), "{err}");
        let started = std::time::Instant::now();
        start_then_confirm(async { Ok(()) }, || async { true }, window)
            .await
            .unwrap();
        assert!(started.elapsed() >= window);
    }
}
