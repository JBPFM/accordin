// SPDX-License-Identifier: GPL-2.0-only

//! Access to the scheduler's admission-routing counters in the mmapped BPF BSS.
//!
//! The scheduler only maintains them when debug counters are enabled, so the
//! loader publishes the pointers in that case only and a null slot means "no
//! evidence available".

use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

pub const ROUTING_COUNTER_COUNT: usize = 9;

/// Counter labels in the order expected by `set_routing_counter_ptrs`.
pub const ROUTING_COUNTER_NAMES: [&str; ROUTING_COUNTER_COUNT] = [
    "select_local_direct",
    "wake_consumed_seen",
    "wake_consumed_granted",
    "wake_consumed_inactive",
    "wake_consumed_normal",
    "wake_read_fail",
    "running_pending_grant_success",
    "running_pending_grant_failure",
    "block_release_read_fail",
];

static ROUTING_COUNTER_PTRS: [AtomicPtr<u64>; ROUTING_COUNTER_COUNT] =
    [const { AtomicPtr::new(ptr::null_mut()) }; ROUTING_COUNTER_COUNT];

#[doc(hidden)]
pub fn set_routing_counter_ptrs(ptrs: [*mut u64; ROUTING_COUNTER_COUNT]) {
    for (slot, ptr) in ROUTING_COUNTER_PTRS.iter().zip(ptrs) {
        slot.store(ptr, Ordering::Release);
    }
}

#[doc(hidden)]
pub fn clear_routing_counter_ptrs() {
    set_routing_counter_ptrs([ptr::null_mut(); ROUTING_COUNTER_COUNT]);
}

/// Volatile snapshot of the counters, `None` while the scheduler is not
/// publishing them.
pub fn routing_counters() -> Option<[u64; ROUTING_COUNTER_COUNT]> {
    let mut values = [0u64; ROUTING_COUNTER_COUNT];

    for (value, slot) in values.iter_mut().zip(ROUTING_COUNTER_PTRS.iter()) {
        let ptr = slot.load(Ordering::Acquire);
        if ptr.is_null() {
            return None;
        }
        *value = unsafe { ptr.read_volatile() };
    }

    Some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_counters_track_published_pointers() {
        assert_eq!(routing_counters(), None);

        let mut storage = [0u64; ROUTING_COUNTER_COUNT];
        for (index, slot) in storage.iter_mut().enumerate() {
            *slot = index as u64;
        }

        let mut ptrs = [ptr::null_mut(); ROUTING_COUNTER_COUNT];
        for (ptr_slot, value) in ptrs.iter_mut().zip(storage.iter_mut()) {
            *ptr_slot = value as *mut u64;
        }
        set_routing_counter_ptrs(ptrs);

        assert_eq!(routing_counters(), Some(storage));

        clear_routing_counter_ptrs();
        assert_eq!(routing_counters(), None);
    }
}
