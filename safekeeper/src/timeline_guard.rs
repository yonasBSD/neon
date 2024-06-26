//! Timeline residence guard is needed to ensure that WAL segments are present on disk,
//! as long as the code is holding the guard. This file implements guard logic, to issue
//! and drop guards, and to notify the manager when the guard is dropped.

use std::collections::HashSet;

use tracing::{debug, warn};

use crate::timeline_manager::ManagerCtlMessage;

#[derive(Debug, Clone, Copy)]
pub struct GuardId(u64);

pub struct ResidenceGuard {
    manager_tx: tokio::sync::mpsc::UnboundedSender<ManagerCtlMessage>,
    guard_id: GuardId,
}

impl Drop for ResidenceGuard {
    fn drop(&mut self) {
        // notify the manager that the guard is dropped
        let res = self
            .manager_tx
            .send(ManagerCtlMessage::GuardDrop(self.guard_id));
        if let Err(e) = res {
            warn!("failed to send GuardDrop message: {:?}", e);
        }
    }
}

/// AccessService is responsible for issuing and dropping residence guards.
/// All guards are stored in the `guards` set.
/// TODO: it's possible to add `String` name to each guard, for better observability.
pub(crate) struct AccessService {
    next_guard_id: u64,
    guards: HashSet<u64>,
    manager_tx: tokio::sync::mpsc::UnboundedSender<ManagerCtlMessage>,
}

impl AccessService {
    pub(crate) fn new(manager_tx: tokio::sync::mpsc::UnboundedSender<ManagerCtlMessage>) -> Self {
        Self {
            next_guard_id: 0,
            guards: HashSet::new(),
            manager_tx,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.guards.is_empty()
    }

    pub(crate) fn create_guard(&mut self) -> ResidenceGuard {
        let guard_id = self.next_guard_id;
        self.next_guard_id += 1;
        self.guards.insert(guard_id);

        let guard_id = GuardId(guard_id);
        debug!("issued a new guard {:?}", guard_id);

        ResidenceGuard {
            manager_tx: self.manager_tx.clone(),
            guard_id,
        }
    }

    pub(crate) fn drop_guard(&mut self, guard_id: GuardId) {
        debug!("dropping guard {:?}", guard_id);
        assert!(self.guards.remove(&guard_id.0));
    }
}
