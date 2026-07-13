use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::stubs::{BrowserWorker, NetworkNamespace, SocksRelay};

#[derive(Debug)]
pub struct Slot {
    pub id: usize,
    busy: AtomicBool,
    pub netns: NetworkNamespace,
    pub relay: SocksRelay,
    pub worker: BrowserWorker,
}

#[derive(Debug)]
pub struct SlotRegistry {
    slots: Vec<Slot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotCounts {
    pub total: usize,
    pub busy: usize,
    pub free: usize,
}

pub struct SlotLease {
    registry: Arc<SlotRegistry>,
    index: usize,
}

impl SlotRegistry {
    pub fn new(count: usize) -> Arc<Self> {
        let count = count.max(1);
        Arc::new(Self {
            slots: (0..count)
                .map(|id| Slot {
                    id,
                    busy: AtomicBool::new(false),
                    netns: NetworkNamespace { slot_id: id },
                    relay: SocksRelay { slot_id: id },
                    worker: BrowserWorker {
                        slot_id: id,
                        profile_dir: std::path::PathBuf::from(format!("profiles/slot-{id}")),
                    },
                })
                .collect(),
        })
    }

    /// Acquire immediately or return `None`; requests never queue behind a full pool.
    pub fn try_acquire(self: &Arc<Self>) -> Option<SlotLease> {
        self.slots.iter().enumerate().find_map(|(index, slot)| {
            slot.busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .ok()
                .map(|_| SlotLease {
                    registry: Arc::clone(self),
                    index,
                })
        })
    }

    pub fn counts(&self) -> SlotCounts {
        let busy = self
            .slots
            .iter()
            .filter(|slot| slot.busy.load(Ordering::Acquire))
            .count();
        SlotCounts {
            total: self.slots.len(),
            busy,
            free: self.slots.len() - busy,
        }
    }
}

impl SlotLease {
    pub fn slot(&self) -> &Slot {
        &self.registry.slots[self.index]
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        self.registry.slots[self.index]
            .busy
            .store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_saturate_and_release() {
        let registry = SlotRegistry::new(1);
        let lease = registry.try_acquire().expect("first slot");
        assert_eq!(lease.slot().id, 0);
        assert_eq!(registry.counts().busy, 1);
        assert!(registry.try_acquire().is_none());
        drop(lease);
        assert_eq!(registry.counts().free, 1);
        assert!(registry.try_acquire().is_some());
    }
}
