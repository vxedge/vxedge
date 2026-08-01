//! Async job engine: every mutating vxAPI RPC submits work here and returns a
//! `Job` immediately; progress is observable via `JobService.Watch` (backed by
//! a `tokio::sync::watch` channel per job) and on the event bus.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, watch};
use vxedge_api::v1::{Job, JobState};

use crate::events::EventBus;

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

/// Handle passed to job bodies for reporting progress.
pub struct Progress {
    tx: watch::Sender<Job>,
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
}

#[derive(Clone)]
pub struct JobEngine {
    inner: Arc<Inner>,
}

struct Inner {
    seq: AtomicU64,
    jobs: Mutex<HashMap<String, watch::Sender<Job>>>,
    events: EventBus,
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

    pub fn new(events: EventBus) -> Self {
        Self {
            inner: Arc::new(Inner {
                seq: AtomicU64::new(0),
                jobs: Mutex::new(HashMap::new()),
                events,
            }),
        }
    }

    /// Submit a job. `work` receives a `Progress` handle; its `Ok(message)` /
    /// `Err(error)` decides the terminal state. Returns the initial snapshot.
    pub async fn submit<F, Fut>(&self, kind: &str, work: F) -> Job
    where
        F: FnOnce(Progress) -> Fut + Send + 'static,
        Fut: Future<Output = Result<String, String>> + Send + 'static,
    {
        let id = format!("job-{:08x}", self.inner.seq.fetch_add(1, Ordering::Relaxed));
        let job = Job {
            id: id.clone(),
            kind: kind.to_string(),
            state: JobState::Pending as i32,
            progress_percent: 0,
            message: String::new(),
            error: String::new(),
            created_unix_ms: now_ms(),
            updated_unix_ms: now_ms(),
        };
        let (tx, _) = watch::channel(job.clone());
        self.inner.jobs.lock().await.insert(id.clone(), tx.clone());

        let events = self.inner.events.clone();
        let job_kind = kind.to_string();
        events.emit(crate::events::EventKind::JobState, &id, "pending");
        tokio::spawn(async move {
            tx.send_modify(|j| {
                j.state = JobState::Running as i32;
                j.updated_unix_ms = now_ms();
            });
            events.emit(crate::events::EventKind::JobState, &id, "running");

            let result = work(Progress { tx: tx.clone() }).await;

            match result {
                Ok(message) => {
                    tx.send_modify(|j| {
                        j.state = JobState::Succeeded as i32;
                        j.progress_percent = 100;
                        j.message = message;
                        j.updated_unix_ms = now_ms();
                    });
                    events.emit(crate::events::EventKind::JobState, &id, "succeeded");
                    tracing::info!(job = %id, kind = %job_kind, "job succeeded");
                }
                Err(error) => {
                    tx.send_modify(|j| {
                        j.state = JobState::Failed as i32;
                        j.error = error.clone();
                        j.updated_unix_ms = now_ms();
                    });
                    events.emit(crate::events::EventKind::JobState, &id, "failed");
                    tracing::warn!(job = %id, kind = %job_kind, error = %error, "job failed");
                }
            }
        });

        job
    }

    pub async fn get(&self, id: &str) -> Option<Job> {
        self.inner
            .jobs
            .lock()
            .await
            .get(id)
            .map(|tx| tx.borrow().clone())
    }

    pub async fn watch(&self, id: &str) -> Option<watch::Receiver<Job>> {
        self.inner
            .jobs
            .lock()
            .await
            .get(id)
            .map(|tx| tx.subscribe())
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

    #[tokio::test]
    async fn job_runs_to_success_with_progress() {
        let engine = JobEngine::new(EventBus::new());
        let job = engine
            .submit("test.ok", |p| async move {
                p.update(50, "halfway");
                Ok("done".to_string())
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
    async fn job_failure_carries_error() {
        let engine = JobEngine::new(EventBus::new());
        let job = engine
            .submit("test.fail", |p| async move {
                p.update(30, "about to fail");
                Err("backend exploded".to_string())
            })
            .await;
        let rx = engine.watch(&job.id).await.expect("job registered");
        let terminal = wait_terminal(rx).await;
        assert_eq!(terminal.state, JobState::Failed as i32);
        assert_eq!(terminal.error, "backend exploded");
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown_job() {
        let engine = JobEngine::new(EventBus::new());
        assert!(engine.get("job-nope").await.is_none());
    }
}
