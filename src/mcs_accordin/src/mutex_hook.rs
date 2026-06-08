// SPDX-License-Identifier: GPL-2.0-only

use crate::lock_backend::LockBackend;
use crate::mcs::McsLockRaw;
use accordin_shared::mutex_hook::MutexHookBackend;

struct McsBackend;

impl MutexHookBackend for McsBackend {
    type LockState = McsLockRaw;
    const USES_ADMISSION_SCOPE: bool = true;

    fn create_state() -> Self::LockState {
        McsLockRaw::new()
    }

    fn lock(state: &Self::LockState) {
        LockBackend::lock(state);
    }

    fn try_lock(state: &Self::LockState) -> bool {
        LockBackend::try_lock(state)
    }

    fn unlock(state: &Self::LockState) {
        LockBackend::unlock(state);
        std::thread::yield_now();
    }
}

accordin_shared::export_mutex_hooks!(super::McsBackend);

#[cfg(test)]
mod tests {
    use accordin_shared::mutex_hook::MutexHookBackend;

    use super::McsBackend;

    #[test]
    fn mcs_backend_uses_per_lock_admission_scope() {
        assert!(<McsBackend as MutexHookBackend>::USES_ADMISSION_SCOPE);
    }

    #[test]
    fn mcs_backend_unlock_does_not_yield() {
        let source = include_str!("mutex_hook.rs");
        let body = source
            .split_once("fn unlock(state: &Self::LockState) {")
            .and_then(|(_, rest)| rest.split_once("\n    }"))
            .map(|(body, _)| body)
            .expect("McsBackend::unlock body should be present");

        assert!(
            !body.contains("yield_now"),
            "McsBackend::unlock should only release the lock"
        );
    }
}
