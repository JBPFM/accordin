// SPDX-License-Identifier: GPL-2.0-only

use crate::lock_backend::LockBackend;
use crate::mcs_tas::McsTasLockRaw;
use lb_shared::mutex_hook::MutexHookBackend;

struct McsTasBackend;

impl MutexHookBackend for McsTasBackend {
    type LockState = McsTasLockRaw;

    fn create_state() -> Self::LockState {
        McsTasLockRaw::new()
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

lb_shared::export_mutex_hooks!(super::McsTasBackend);
