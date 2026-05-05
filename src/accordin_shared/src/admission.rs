use std::cell::Cell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const IN_CRITICAL_SECTION: u32 = 1 << 0;
const SLOW_PATH_PENDING: u32 = 1 << 1;

#[repr(C)]
pub struct UserAdmissionCtx {
    flags: AtomicU32,
    lock_domain: AtomicU32,
    lock_id: AtomicU64,
    tracked_lock_depth: AtomicU32,
}

impl UserAdmissionCtx {
    const fn new() -> Self {
        Self {
            flags: AtomicU32::new(0),
            lock_domain: AtomicU32::new(0),
            lock_id: AtomicU64::new(0),
            tracked_lock_depth: AtomicU32::new(0),
        }
    }
}

thread_local! {
    static USER_ADMISSION_CTX: UserAdmissionCtx = const { UserAdmissionCtx::new() };
    static RAW_ENTERED_PENDING: Cell<bool> = const { Cell::new(false) };
}

pub fn user_word_addr() -> *const u32 {
    USER_ADMISSION_CTX.with(|ctx| &ctx.flags as *const AtomicU32 as *const u32)
}

pub fn user_ctx_addr() -> *const UserAdmissionCtx {
    USER_ADMISSION_CTX.with(|ctx| ctx as *const UserAdmissionCtx)
}

#[inline(always)]
pub fn tracked_lock_depth() -> u32 {
    USER_ADMISSION_CTX.with(|ctx| ctx.tracked_lock_depth.load(Ordering::Relaxed))
}

#[inline(always)]
pub fn mark_slow_path_pending() {
    USER_ADMISSION_CTX.with(|ctx| {
        if ctx.tracked_lock_depth.load(Ordering::Relaxed) != 0 {
            return;
        }
        let value = ctx.flags.load(Ordering::Relaxed);
        ctx.flags
            .store(value | SLOW_PATH_PENDING, Ordering::Release);
    });
}

#[inline(always)]
pub fn mark_slow_path_pending_for_lock(lock_domain: u32, lock_id: u64) {
    USER_ADMISSION_CTX.with(|ctx| {
        if ctx.tracked_lock_depth.load(Ordering::Relaxed) != 0 {
            return;
        }
        ctx.lock_id.store(lock_id, Ordering::Relaxed);
        ctx.lock_domain.store(lock_domain, Ordering::Relaxed);
        let value = ctx.flags.load(Ordering::Relaxed);
        ctx.flags
            .store(value | SLOW_PATH_PENDING, Ordering::Release);
    });
}

#[inline(always)]
pub fn mark_critical_section_entered() {
    mark_critical_section_entered_inner(true);
}

#[inline(always)]
pub fn mark_critical_section_entered_from_hook() {
    if RAW_ENTERED_PENDING.with(|pending| pending.replace(false)) {
        USER_ADMISSION_CTX.with(|ctx| {
            let value = ctx.flags.load(Ordering::Relaxed);
            ctx.flags.store(
                (value | IN_CRITICAL_SECTION) & !SLOW_PATH_PENDING,
                Ordering::Release,
            );
        });
        return;
    }

    mark_critical_section_entered_inner(false);
}

#[inline(always)]
fn mark_critical_section_entered_inner(raw_mark: bool) {
    USER_ADMISSION_CTX.with(|ctx| {
        ctx.tracked_lock_depth.fetch_add(1, Ordering::Relaxed);
        let value = ctx.flags.load(Ordering::Relaxed);
        ctx.flags.store(
            (value | IN_CRITICAL_SECTION) & !SLOW_PATH_PENDING,
            Ordering::Release,
        );
    });
    if raw_mark {
        RAW_ENTERED_PENDING.with(|pending| pending.set(true));
    }
}

#[inline(always)]
pub fn mark_critical_section_exit() {
    USER_ADMISSION_CTX.with(|ctx| {
        let depth = ctx.tracked_lock_depth.load(Ordering::Relaxed);
        let next_depth = depth.saturating_sub(1);
        ctx.tracked_lock_depth.store(next_depth, Ordering::Relaxed);

        let mut value = ctx.flags.load(Ordering::Relaxed) & !SLOW_PATH_PENDING;
        if next_depth == 0 {
            value &= !IN_CRITICAL_SECTION;
            ctx.lock_domain.store(0, Ordering::Relaxed);
            ctx.lock_id.store(0, Ordering::Relaxed);
        } else {
            value |= IN_CRITICAL_SECTION;
        }
        ctx.flags.store(value, Ordering::Release);
    });
    RAW_ENTERED_PENDING.with(|pending| pending.set(false));
}

#[inline(always)]
pub fn reset_state() {
    USER_ADMISSION_CTX.with(|ctx| {
        ctx.flags.store(0, Ordering::Relaxed);
        ctx.lock_domain.store(0, Ordering::Relaxed);
        ctx.lock_id.store(0, Ordering::Relaxed);
        ctx.tracked_lock_depth.store(0, Ordering::Relaxed);
    });
    RAW_ENTERED_PENDING.with(|pending| pending.set(false));
}

#[doc(hidden)]
pub fn word_for_test() -> u32 {
    USER_ADMISSION_CTX.with(|ctx| ctx.flags.load(Ordering::Relaxed))
}

#[doc(hidden)]
pub fn lock_domain_for_test() -> u32 {
    USER_ADMISSION_CTX.with(|ctx| ctx.lock_domain.load(Ordering::Relaxed))
}

#[doc(hidden)]
pub fn lock_id_for_test() -> u64 {
    USER_ADMISSION_CTX.with(|ctx| ctx.lock_id.load(Ordering::Relaxed))
}

#[doc(hidden)]
pub fn tracked_lock_depth_for_test() -> u32 {
    tracked_lock_depth()
}

#[cfg(test)]
mod tests {
    use super::{
        IN_CRITICAL_SECTION, SLOW_PATH_PENDING, mark_critical_section_entered,
        mark_critical_section_entered_from_hook, mark_critical_section_exit,
        mark_slow_path_pending, mark_slow_path_pending_for_lock, reset_state,
        tracked_lock_depth_for_test, word_for_test,
    };

    #[test]
    fn admission_word_helpers_track_bit_transitions() {
        reset_state();

        mark_slow_path_pending();
        assert_eq!(word_for_test(), SLOW_PATH_PENDING);

        mark_critical_section_entered();
        assert_eq!(word_for_test(), IN_CRITICAL_SECTION);
        assert_eq!(tracked_lock_depth_for_test(), 1);

        mark_slow_path_pending();
        assert_eq!(word_for_test(), IN_CRITICAL_SECTION);

        mark_critical_section_exit();
        assert_eq!(word_for_test(), 0);
        assert_eq!(tracked_lock_depth_for_test(), 0);

        mark_critical_section_entered();
        mark_critical_section_exit();
        assert_eq!(word_for_test(), 0);
    }

    #[test]
    fn slow_path_can_publish_lock_domain() {
        reset_state();

        mark_slow_path_pending_for_lock(7, 0x1234);

        assert_eq!(word_for_test(), SLOW_PATH_PENDING);
        assert_eq!(super::lock_domain_for_test(), 7);
        assert_eq!(super::lock_id_for_test(), 0x1234);
    }

    #[test]
    fn held_lock_depth_bypasses_new_slow_path_gate() {
        reset_state();
        mark_critical_section_entered();

        mark_slow_path_pending_for_lock(3, 99);

        assert_eq!(word_for_test(), IN_CRITICAL_SECTION);
        assert_eq!(super::lock_domain_for_test(), 0);
        assert_eq!(tracked_lock_depth_for_test(), 1);

        mark_critical_section_exit();
    }

    #[test]
    fn hook_entry_does_not_double_count_raw_entry() {
        reset_state();

        mark_critical_section_entered();
        mark_critical_section_entered_from_hook();

        assert_eq!(tracked_lock_depth_for_test(), 1);

        mark_critical_section_exit();
        assert_eq!(tracked_lock_depth_for_test(), 0);
    }
}
