use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::pipe::leak::LeakGate;
use crate::pipe::upstream::UpstreamMap;
use crate::pipe::{PipeConfig, PipeSlot};

#[derive(Debug)]
pub struct Slot {
    pub id: usize,
    busy: AtomicBool,
    pub pipe: Option<Arc<PipeSlot>>,
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
    pub quarantined: usize,
}

pub struct SlotLease {
    registry: Arc<SlotRegistry>,
    index: usize,
}

impl SlotRegistry {
    /// Provision a production pool. All slots share the source-keyed upstream
    /// map and leak gate, while each owns a stable relay port and namespace.
    pub async fn provision(count: usize, config: &PipeConfig) -> Result<Arc<Self>, String> {
        let count = count.max(1);
        let upstreams = UpstreamMap::default();
        let gate = LeakGate::default();
        let mut slots = Vec::with_capacity(count);
        for id in 0..count {
            let pipe = PipeSlot::provision(id, config, upstreams.clone(), gate.clone()).await?;
            slots.push(Slot {
                id,
                busy: AtomicBool::new(false),
                pipe: Some(pipe),
            });
        }
        Ok(Arc::new(Self { slots }))
    }

    /// Unprovisioned registry for protocol/router unit tests. Production daemon
    /// startup uses [`Self::provision`] and refuses to serve partial pools.
    pub fn new(count: usize) -> Arc<Self> {
        let count = count.max(1);
        Arc::new(Self {
            slots: (0..count)
                .map(|id| Slot {
                    id,
                    busy: AtomicBool::new(false),
                    pipe: None,
                })
                .collect(),
        })
    }

    pub fn try_acquire(self: &Arc<Self>) -> Option<SlotLease> {
        self.slots.iter().enumerate().find_map(|(index, slot)| {
            if slot.pipe.as_ref().is_some_and(|pipe| {
                matches!(
                    pipe.state(),
                    crate::pipe::leak::SlotServiceState::Quarantined(_)
                )
            }) {
                return None;
            }
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
        let quarantined = self
            .slots
            .iter()
            .filter(|slot| {
                slot.pipe.as_ref().is_some_and(|pipe| {
                    matches!(
                        pipe.state(),
                        crate::pipe::leak::SlotServiceState::Quarantined(_)
                    )
                })
            })
            .count();
        let free = self
            .slots
            .iter()
            .filter(|slot| {
                !slot.busy.load(Ordering::Acquire)
                    && !slot.pipe.as_ref().is_some_and(|pipe| {
                        matches!(
                            pipe.state(),
                            crate::pipe::leak::SlotServiceState::Quarantined(_)
                        )
                    })
            })
            .count();
        SlotCounts {
            total: self.slots.len(),
            busy,
            free,
            quarantined,
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
        let slot = &self.registry.slots[self.index];
        if let Some(pipe) = &slot.pipe {
            pipe.release();
        }
        slot.busy.store(false, Ordering::Release);
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
