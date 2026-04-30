// SPDX-License-Identifier: GPL-2.0-only

use crate::lock_backend::LockBackend;
use crate::mcs::McsLockRaw;
use lb_shared::mutex_hook::{MutexHookBackend, ThreadRegistration};
use lb_shared::timeslice_extension::{CriticalSectionTimesliceExtension, TimesliceExtensionMode};

struct McsTseLockState {
    lock: McsLockRaw,
    timeslice: CriticalSectionTimesliceExtension,
}

impl McsTseLockState {
    fn new() -> Self {
        Self {
            lock: McsLockRaw::new(),
            timeslice: CriticalSectionTimesliceExtension::new(TimesliceExtensionMode::Require),
        }
    }
}

struct McsTseBackend;

impl MutexHookBackend for McsTseBackend {
    type LockState = McsTseLockState;

    fn create_state() -> Self::LockState {
        McsTseLockState::new()
    }

    fn lock(state: &Self::LockState) {
        state.timeslice.prepare_thread();
        LockBackend::lock(&state.lock);
        state.timeslice.on_critical_section_enter();
    }

    fn try_lock(state: &Self::LockState) -> bool {
        state.timeslice.prepare_thread();
        if !LockBackend::try_lock(&state.lock) {
            return false;
        }
        state.timeslice.on_critical_section_enter();
        true
    }

    fn unlock(state: &Self::LockState) {
        LockBackend::unlock(&state.lock);
        state.timeslice.on_critical_section_exit();
    }
}

struct NoThreadRegistration;

impl ThreadRegistration for NoThreadRegistration {
    fn register_current_thread() -> bool {
        true
    }

    fn unregister_current_thread() {}
}

lb_shared::export_mutex_hooks!(McsTseBackend, NoThreadRegistration);
