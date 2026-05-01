// SPDX-License-Identifier: GPL-2.0-only

use crate::lock_backend::LockBackend;
use crate::ttas::TtasLockRaw;
use lb_shared::mutex_hook::MutexHookBackend;

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

lb_shared::export_mutex_hooks!(super::TtasBackend);
