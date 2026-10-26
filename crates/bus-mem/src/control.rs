//! Simulation control for [`crate::MemBus`]: virtual clock, faults, restarts.

use std::sync::Arc;
use std::time::Duration;

use a2a_types::ids::TenantId;
use bus_api::control::{BusControl, Fault};
use bus_api::name::{ConsumerName, StreamName};
use bus_api::value::Digest;
use bytes::Bytes;

use crate::state::{Faults, Shared};

/// Control handle for a [`crate::MemBus`]. Cheap to clone; all clones act on
/// the same bus.
#[derive(Debug, Clone)]
pub struct MemControl {
    shared: Arc<Shared>,
}

impl MemControl {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }
}

impl BusControl for MemControl {
    fn seed(&self) -> u64 {
        self.shared.lock().seed
    }

    fn now(&self) -> Duration {
        self.shared.lock().now
    }

    fn advance(&self, by: Duration) {
        {
            let mut inner = self.shared.lock();
            inner.now = inner.now.saturating_add(by);
        }
        self.shared.wake();
    }

    fn set_fault(&self, fault: Fault) {
        {
            let mut inner = self.shared.lock();
            match fault {
                Fault::Drop(p) => inner.faults.drop = Some(p.get()),
                Fault::Duplicate(p) => inner.faults.duplicate = Some(p.get()),
                Fault::Reorder { window } => inner.faults.reorder = Some(window),
                Fault::Delay(dist) => inner.faults.delay = Some(dist),
                Fault::Partition { set_a, set_b } => {
                    inner.faults.partition = Some((set_a, set_b));
                }
            }
        }
        self.shared.wake();
    }

    fn clear_faults(&self) {
        self.shared.lock().faults = Faults::default();
        self.shared.wake();
    }

    fn fault_trace(&self) -> Vec<String> {
        self.shared.lock().trace.clone()
    }

    fn restart_bus(&self) {
        self.shared.lock().restart();
        self.shared.wake();
    }

    fn crash_consumer(&self, stream: &StreamName, consumer: &ConsumerName) -> bool {
        let found = self
            .shared
            .lock()
            .crash_consumer(&(stream.clone(), consumer.clone()));
        self.shared.wake();
        found
    }

    fn corrupt_blob(&self, tenant: &TenantId, digest: &Digest) -> bool {
        let mut inner = self.shared.lock();
        let Some(existing) = inner.blobs.get(&(tenant.clone(), *digest)).cloned() else {
            return false;
        };
        let mut bytes = existing.to_vec();
        match bytes.first_mut() {
            Some(first) => *first ^= 0xFF,
            None => bytes.push(0xFF),
        }
        inner
            .blobs
            .insert((tenant.clone(), *digest), Bytes::from(bytes));
        true
    }
}
