//! In-process event bus: every state transition and audit-worthy action is
//! published here; `EventService.Subscribe` streams it to clients.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::broadcast;
use vxedge_api::v1::{Event, ResourceKind};

const BUS_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    seq: Arc<AtomicU64>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self {
            tx,
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn emit(&self, kind: &str, resource_kind: ResourceKind, resource_vxid: &str, detail: &str) {
        let event = Event {
            id: format!("evt-{:08x}", self.seq.fetch_add(1, Ordering::Relaxed)),
            ts_unix_ms: crate::jobs::now_ms(),
            kind: kind.to_string(),
            resource_kind: resource_kind as i32,
            resource_vxid: resource_vxid.to_string(),
            detail: detail.to_string(),
        };
        // No subscribers is fine; the bus is fire-and-forget.
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}
