// SPDX-License-Identifier: GPL-2.0-only

use std::sync::OnceLock;

use crate::flexguard::FlexguardLockRaw;
use crate::lock_stats::thread_ctx;
use lb_shared::mutex_hook::{MutexHookBackend, ThreadRegistration, current_tid};
use libbpf_rs::{MapCore, MapFlags, MapHandle};

static THREAD_CTX_MAP: OnceLock<MapHandle> = OnceLock::new();
static NODES_MAP: OnceLock<MapHandle> = OnceLock::new();

pub fn set_thread_ctx_map(map: MapHandle) {
    let _ = THREAD_CTX_MAP.set(map);
}

pub fn set_nodes_map(map: MapHandle) {
    let _ = NODES_MAP.set(map);
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

fn register_thread_with_maps<T, U>(
    thread_ctx_map: &T,
    nodes_map: &U,
    tid: u32,
    ctx_ptr: u64,
    thread_index: u32,
) -> bool
where
    T: ThreadCtxMapOps + ?Sized,
    U: ThreadCtxMapOps + ?Sized,
{
    thread_ctx_map.update_entry(&tid.to_ne_bytes(), &ctx_ptr.to_ne_bytes(), MapFlags::ANY)
        && nodes_map.update_entry(
            &tid.to_ne_bytes(),
            &thread_index.to_ne_bytes(),
            MapFlags::ANY,
        )
}

fn unregister_thread_with_maps<T, U>(thread_ctx_map: &T, nodes_map: &U, tid: u32) -> bool
where
    T: ThreadCtxMapOps + ?Sized,
    U: ThreadCtxMapOps + ?Sized,
{
    thread_ctx_map.delete_entry(&tid.to_ne_bytes()) && nodes_map.delete_entry(&tid.to_ne_bytes())
}

struct FlexguardBackend;

impl MutexHookBackend for FlexguardBackend {
    type LockState = FlexguardLockRaw;

    fn create_state() -> Self::LockState {
        FlexguardLockRaw::new()
    }

    fn lock(state: &Self::LockState) {
        state.lock();
    }

    fn try_lock(state: &Self::LockState) -> bool {
        state.try_lock()
    }

    fn unlock(state: &Self::LockState) {
        state.unlock();
    }
}

struct FlexguardRegistration;

impl ThreadRegistration for FlexguardRegistration {
    fn register_current_thread() -> bool {
        let (Some(thread_ctx_map), Some(nodes_map)) = (THREAD_CTX_MAP.get(), NODES_MAP.get())
        else {
            return false;
        };

        let tid = current_tid();
        let ctx_ptr = thread_ctx() as u64;
        let thread_index = crate::flexguard::current_thread_index() as u32;
        register_thread_with_maps(thread_ctx_map, nodes_map, tid, ctx_ptr, thread_index)
    }

    fn unregister_current_thread() {
        let (Some(thread_ctx_map), Some(nodes_map)) = (THREAD_CTX_MAP.get(), NODES_MAP.get())
        else {
            return;
        };

        let _ = unregister_thread_with_maps(thread_ctx_map, nodes_map, current_tid());
    }
}

lb_shared::export_mutex_hooks!(FlexguardBackend, FlexguardRegistration);

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use libbpf_rs::MapFlags;

    use super::{ThreadCtxMapOps, register_thread_with_maps, unregister_thread_with_maps};

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
    fn register_thread_updates_scheduler_and_flexguard_maps() {
        let thread_ctx_map = RecordingMap::default();
        let nodes_map = RecordingMap::default();

        assert!(register_thread_with_maps(
            &thread_ctx_map,
            &nodes_map,
            7,
            0x1122_3344_5566_7788,
            13,
        ));

        assert_eq!(
            thread_ctx_map.calls(),
            vec![MapCall::Update {
                key: 7u32.to_ne_bytes().to_vec(),
                value: 0x1122_3344_5566_7788u64.to_ne_bytes().to_vec(),
                flags: MapFlags::ANY,
            }]
        );
        assert_eq!(
            nodes_map.calls(),
            vec![MapCall::Update {
                key: 7u32.to_ne_bytes().to_vec(),
                value: 13u32.to_ne_bytes().to_vec(),
                flags: MapFlags::ANY,
            }]
        );
    }

    #[test]
    fn unregister_thread_removes_scheduler_and_flexguard_maps() {
        let thread_ctx_map = RecordingMap::default();
        let nodes_map = RecordingMap::default();

        assert!(unregister_thread_with_maps(&thread_ctx_map, &nodes_map, 11,));

        assert_eq!(
            thread_ctx_map.calls(),
            vec![MapCall::Delete {
                key: 11u32.to_ne_bytes().to_vec(),
            }]
        );
        assert_eq!(
            nodes_map.calls(),
            vec![MapCall::Delete {
                key: 11u32.to_ne_bytes().to_vec(),
            }]
        );
    }
}
