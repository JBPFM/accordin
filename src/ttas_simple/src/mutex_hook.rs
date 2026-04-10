// SPDX-License-Identifier: GPL-2.0-only
//
// Interpose pthread mutex/cond and back them with a TTAS lock.

use crate::lock_backend::LockBackend;
use crate::lock_stats::thread_ctx;
use crate::ttas::TtasLockRaw;
use lb_shared::mutex_hook::{MutexHookBackend, ThreadRegistration, current_tid};
use libbpf_rs::{MapCore, MapFlags, MapHandle};
use std::sync::OnceLock;

static THREAD_CTX_MAP: OnceLock<MapHandle> = OnceLock::new();

pub fn set_thread_ctx_map(map: MapHandle) {
    let _ = THREAD_CTX_MAP.set(map);
}

trait ThreadCtxMapOps {
    fn update_entry(&self, key: &[u8], value: &[u8], flags: MapFlags) -> bool;
    fn delete_entry(&self, key: &[u8]) -> bool;
}

impl ThreadCtxMapOps for MapHandle {
    fn update_entry(&self, key: &[u8], value: &[u8], flags: MapFlags) -> bool {
        self.update(key, value, flags).is_ok()
    }

    fn delete_entry(&self, key: &[u8]) -> bool {
        self.delete(key).is_ok()
    }
}

fn register_thread_ctx_with_map<M>(map: &M, tid: u32, ctx_ptr: u64) -> bool
where
    M: ThreadCtxMapOps + ?Sized,
{
    map.update_entry(&tid.to_ne_bytes(), &ctx_ptr.to_ne_bytes(), MapFlags::ANY)
}

fn unregister_thread_ctx_with_map<M>(map: &M, tid: u32) -> bool
where
    M: ThreadCtxMapOps + ?Sized,
{
    map.delete_entry(&tid.to_ne_bytes())
}

struct TtasBackend;

impl MutexHookBackend for TtasBackend {
    type LockState = TtasLockRaw;

    fn create_state() -> Self::LockState {
        TtasLockRaw::new()
    }

    fn lock(state: &Self::LockState) {
        LockBackend::lock(state);
    }

    fn try_lock(state: &Self::LockState) -> bool {
        LockBackend::try_lock(state)
    }

    fn unlock(state: &Self::LockState) {
        LockBackend::unlock(state);
    }
}

struct ThreadCtxRegistration;

impl ThreadRegistration for ThreadCtxRegistration {
    fn register_current_thread() -> bool {
        let Some(map) = THREAD_CTX_MAP.get() else {
            return false;
        };

        let tid = current_tid();
        let ctx_ptr = thread_ctx() as u64;
        register_thread_ctx_with_map(map, tid, ctx_ptr)
    }

    fn unregister_current_thread() {
        let Some(map) = THREAD_CTX_MAP.get() else {
            return;
        };

        let _ = unregister_thread_ctx_with_map(map, current_tid());
    }
}

lb_shared::export_mutex_hooks!(TtasBackend, ThreadCtxRegistration);

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use libbpf_rs::MapFlags;

    use super::{ThreadCtxMapOps, register_thread_ctx_with_map, unregister_thread_ctx_with_map};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum MapCall {
        Update {
            key: Vec<u8>,
            value: Vec<u8>,
            flags: MapFlags,
        },
        Delete {
            key: Vec<u8>,
        },
    }

    #[derive(Default)]
    struct RecordingMap {
        calls: RefCell<Vec<MapCall>>,
    }

    impl ThreadCtxMapOps for RecordingMap {
        fn update_entry(&self, key: &[u8], value: &[u8], flags: MapFlags) -> bool {
            self.calls.borrow_mut().push(MapCall::Update {
                key: key.to_vec(),
                value: value.to_vec(),
                flags,
            });
            true
        }

        fn delete_entry(&self, key: &[u8]) -> bool {
            self.calls
                .borrow_mut()
                .push(MapCall::Delete { key: key.to_vec() });
            true
        }
    }

    impl RecordingMap {
        fn calls(&self) -> Vec<MapCall> {
            self.calls.borrow().clone()
        }
    }

    #[test]
    fn register_thread_ctx_uses_map_helper_update() {
        let map = RecordingMap::default();

        assert!(register_thread_ctx_with_map(&map, 7, 0x1122_3344_5566_7788,));

        assert_eq!(
            map.calls(),
            vec![MapCall::Update {
                key: 7u32.to_ne_bytes().to_vec(),
                value: 0x1122_3344_5566_7788u64.to_ne_bytes().to_vec(),
                flags: MapFlags::ANY,
            }]
        );
    }

    #[test]
    fn unregister_thread_ctx_uses_map_helper_delete() {
        let map = RecordingMap::default();

        assert!(unregister_thread_ctx_with_map(&map, 11));

        assert_eq!(
            map.calls(),
            vec![MapCall::Delete {
                key: 11u32.to_ne_bytes().to_vec(),
            }]
        );
    }
}
