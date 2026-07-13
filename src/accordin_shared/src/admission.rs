use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

const IN_CRITICAL_SECTION: u32 = 1 << 0;
const SLOW_PATH_PENDING: u32 = 1 << 1;
const TOKEN_CONSUMED: u32 = 1 << 2;
pub const USER_ADMISSION_FLAG_MASK: u32 = 0x7;
pub const USER_ADMISSION_LOCK_ID_SHIFT: u32 = 3;
pub const MAX_LOCK_CLASSES: u32 = 16;
pub const UNMANAGED_LOCK_ID: u32 = 0;
pub const DISABLE_ADMISSION_ENV: &str = "ACCORDIN_DISABLE_ADMISSION";

static NEXT_LOCK_ID: AtomicU32 = AtomicU32::new(1);
static INACTIVE_ENQUEUE_SEQ_PTR: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
static INACTIVE_EMPTY_SEQ_PTR: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());

thread_local! {
    static USER_ADMISSION_WORD: AtomicU32 = const { AtomicU32::new(0) };
    static THREAD_HELD_DEPTH: Cell<u32> = const { Cell::new(0) };
}

#[derive(Clone, Copy)]
pub struct LockAdmissionScope {
    lock_id: u32,
    outer_managed: bool,
}

pub fn user_word_addr() -> *const u32 {
    USER_ADMISSION_WORD.with(|word| word as *const AtomicU32 as *const u32)
}

#[inline(always)]
fn admission_enabled() -> bool {
    static ADMISSION_ENABLED: OnceLock<bool> = OnceLock::new();

    *ADMISSION_ENABLED.get_or_init(|| !crate::env::env_flag(DISABLE_ADMISSION_ENV))
}

#[inline(always)]
fn managed_lock_id(lock_id: u32) -> bool {
    lock_id != UNMANAGED_LOCK_ID && lock_id < MAX_LOCK_CLASSES
}

#[inline(always)]
fn flags(value: u32) -> u32 {
    value & USER_ADMISSION_FLAG_MASK
}

#[inline(always)]
fn lock_id_bits(value: u32) -> u32 {
    value & !USER_ADMISSION_FLAG_MASK
}

#[inline(always)]
fn user_lock_id_for_value(value: u32) -> u32 {
    lock_id_bits(value) >> USER_ADMISSION_LOCK_ID_SHIFT
}

#[inline(always)]
fn word_with_lock_id(lock_id: u32, flags: u32) -> u32 {
    (lock_id << USER_ADMISSION_LOCK_ID_SHIFT) | (flags & USER_ADMISSION_FLAG_MASK)
}

pub fn allocate_lock_id() -> u32 {
    loop {
        let current = NEXT_LOCK_ID.load(Ordering::Relaxed);
        if current >= MAX_LOCK_CLASSES {
            return UNMANAGED_LOCK_ID;
        }

        if NEXT_LOCK_ID
            .compare_exchange_weak(current, current + 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return current;
        }
    }
}

#[doc(hidden)]
pub fn set_inactive_queue_seq_ptrs(enqueue_seq: *mut u32, empty_seq: *mut u32) {
    INACTIVE_ENQUEUE_SEQ_PTR.store(enqueue_seq, Ordering::Release);
    INACTIVE_EMPTY_SEQ_PTR.store(empty_seq, Ordering::Release);
}

#[inline(always)]
fn inactive_queue_idle() -> Option<bool> {
    let enqueue_seq = INACTIVE_ENQUEUE_SEQ_PTR.load(Ordering::Acquire);
    let empty_seq = INACTIVE_EMPTY_SEQ_PTR.load(Ordering::Acquire);
    if enqueue_seq.is_null() || empty_seq.is_null() {
        return None;
    }

    let enqueue_seq = unsafe { enqueue_seq.read_volatile() };
    let empty_seq = unsafe { empty_seq.read_volatile() };
    Some(enqueue_seq == empty_seq)
}

#[inline(always)]
fn slow_path_yield_required(token_consumed: bool) -> bool {
    if token_consumed && inactive_queue_idle().is_some_and(|idle| idle) {
        return false;
    }

    true
}

#[inline(always)]
pub fn begin_lock_scope(lock_id: u32) -> LockAdmissionScope {
    THREAD_HELD_DEPTH.with(|depth| {
        let held = depth.get();
        depth.set(held.saturating_add(1));
        LockAdmissionScope {
            lock_id,
            outer_managed: held == 0 && managed_lock_id(lock_id),
        }
    })
}

#[inline(always)]
pub fn mark_slow_path_pending_for_scope(scope: LockAdmissionScope) -> bool {
    if scope.outer_managed {
        mark_slow_path_pending_for_lock(scope.lock_id)
    } else {
        false
    }
}

#[inline(always)]
pub fn prepare_slow_path_admission() {
    let consumed = token_consumed();
    if mark_slow_path_pending() && slow_path_yield_required(consumed) {
        std::thread::yield_now();
        clear_token_consumed();
    }
}

#[inline(always)]
pub fn prepare_slow_path_admission_for_scope(scope: LockAdmissionScope) {
    let consumed = token_consumed_for_scope(scope);
    if mark_slow_path_pending_for_scope(scope) && slow_path_yield_required(consumed) {
        std::thread::yield_now();
        clear_token_consumed_for_scope(scope);
    }
}

#[inline(always)]
pub fn token_consumed_for_scope(scope: LockAdmissionScope) -> bool {
    scope.outer_managed && token_consumed_for_lock(scope.lock_id)
}

#[inline(always)]
pub fn clear_token_consumed_for_scope(scope: LockAdmissionScope) {
    if scope.outer_managed {
        clear_token_consumed_for_lock(scope.lock_id);
    }
}

#[inline(always)]
pub fn mark_critical_section_entered_for_scope(scope: LockAdmissionScope) {
    if scope.outer_managed {
        mark_critical_section_entered_for_lock(scope.lock_id);
    }
}

#[inline(always)]
pub fn finish_lock_scope(lock_id: u32) {
    THREAD_HELD_DEPTH.with(|depth| {
        let held = depth.get();
        if held == 0 {
            return;
        }

        if held == 1 && managed_lock_id(lock_id) {
            mark_critical_section_exit_for_lock(lock_id);
        }
        depth.set(held - 1);
    });
}

#[inline(always)]
pub fn mark_slow_path_pending() -> bool {
    mark_slow_path_pending_with_admission_enabled(admission_enabled())
}

#[inline(always)]
pub fn mark_slow_path_pending_for_lock(lock_id: u32) -> bool {
    mark_slow_path_pending_for_lock_with_admission_enabled(lock_id, admission_enabled())
}

#[inline(always)]
pub fn mark_cond_reacquire_pending_for_lock(lock_id: u32) -> bool {
    mark_cond_reacquire_pending_for_lock_with_admission_enabled(lock_id, admission_enabled())
}

#[inline(always)]
pub fn token_consumed() -> bool {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        flags(value) & TOKEN_CONSUMED != 0
    })
}

#[inline(always)]
fn token_consumed_for_lock(lock_id: u32) -> bool {
    if !managed_lock_id(lock_id) {
        return false;
    }

    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        user_lock_id_for_value(value) == lock_id && flags(value) & TOKEN_CONSUMED != 0
    })
}

#[inline(always)]
fn mark_slow_path_pending_with_admission_enabled(enabled: bool) -> bool {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            value | SLOW_PATH_PENDING
        } else {
            value & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION | TOKEN_CONSUMED)
        };
        word.store(next, Ordering::Relaxed);
    });
    enabled
}

#[inline(always)]
fn mark_slow_path_pending_for_lock_with_admission_enabled(lock_id: u32, enabled: bool) -> bool {
    if !managed_lock_id(lock_id) {
        return false;
    }

    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            let preserved_flags = if user_lock_id_for_value(value) == lock_id {
                flags(value) & TOKEN_CONSUMED
            } else {
                0
            };
            word_with_lock_id(lock_id, preserved_flags | SLOW_PATH_PENDING)
        } else {
            word_with_lock_id(
                lock_id,
                flags(value) & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION | TOKEN_CONSUMED),
            )
        };
        word.store(next, Ordering::Relaxed);
    });
    enabled
}

#[inline(always)]
fn mark_cond_reacquire_pending_for_lock_with_admission_enabled(
    lock_id: u32,
    enabled: bool,
) -> bool {
    if !managed_lock_id(lock_id) {
        return false;
    }

    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            word_with_lock_id(
                lock_id,
                (flags(value) | SLOW_PATH_PENDING | TOKEN_CONSUMED) & !IN_CRITICAL_SECTION,
            )
        } else {
            word_with_lock_id(
                lock_id,
                flags(value) & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION | TOKEN_CONSUMED),
            )
        };
        word.store(next, Ordering::Relaxed);
    });
    enabled
}

#[inline(always)]
pub fn mark_critical_section_entered() {
    mark_critical_section_entered_with_admission_enabled(admission_enabled());
}

#[inline(always)]
pub fn mark_critical_section_entered_for_lock(lock_id: u32) {
    mark_critical_section_entered_for_lock_with_admission_enabled(lock_id, admission_enabled());
}

#[inline(always)]
fn mark_critical_section_entered_with_admission_enabled(enabled: bool) {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            (value | IN_CRITICAL_SECTION) & !(SLOW_PATH_PENDING | TOKEN_CONSUMED)
        } else {
            value & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION | TOKEN_CONSUMED)
        };
        word.store(next, Ordering::Relaxed);
    });
}

#[inline(always)]
fn mark_critical_section_entered_for_lock_with_admission_enabled(lock_id: u32, enabled: bool) {
    if !managed_lock_id(lock_id) {
        return;
    }

    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            word_with_lock_id(
                lock_id,
                (flags(value) | IN_CRITICAL_SECTION) & !(SLOW_PATH_PENDING | TOKEN_CONSUMED),
            )
        } else {
            word_with_lock_id(
                lock_id,
                flags(value) & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION | TOKEN_CONSUMED),
            )
        };
        word.store(next, Ordering::Relaxed);
    });
}

#[inline(always)]
pub fn mark_critical_section_exit() {
    mark_critical_section_exit_with_admission_enabled(admission_enabled());
}

#[inline(always)]
pub fn mark_critical_section_exit_for_lock(lock_id: u32) {
    mark_critical_section_exit_for_lock_with_admission_enabled(lock_id, admission_enabled());
}

#[inline(always)]
fn mark_critical_section_exit_with_admission_enabled(enabled: bool) {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            (value | TOKEN_CONSUMED) & !IN_CRITICAL_SECTION
        } else {
            value & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION | TOKEN_CONSUMED)
        };
        word.store(next, Ordering::Relaxed);
    });
}

#[inline(always)]
fn mark_critical_section_exit_for_lock_with_admission_enabled(lock_id: u32, enabled: bool) {
    if !managed_lock_id(lock_id) {
        return;
    }

    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            word_with_lock_id(
                lock_id,
                (flags(value) | TOKEN_CONSUMED) & !IN_CRITICAL_SECTION,
            )
        } else {
            word_with_lock_id(
                lock_id,
                flags(value) & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION | TOKEN_CONSUMED),
            )
        };
        word.store(next, Ordering::Relaxed);
    });
}

#[inline(always)]
pub fn clear_token_consumed() {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(value & !TOKEN_CONSUMED, Ordering::Relaxed);
    });
}

#[inline(always)]
fn clear_token_consumed_for_lock(lock_id: u32) {
    if !managed_lock_id(lock_id) {
        return;
    }

    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(
            word_with_lock_id(lock_id, flags(value) & !TOKEN_CONSUMED),
            Ordering::Relaxed,
        );
    });
}

#[inline(always)]
pub fn reset_state() {
    USER_ADMISSION_WORD.with(|word| {
        word.store(0, Ordering::Relaxed);
    });
}

#[inline(always)]
pub fn reset_transient_state() {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(lock_id_bits(value), Ordering::Relaxed);
    });
}

#[doc(hidden)]
pub fn word_for_test() -> u32 {
    USER_ADMISSION_WORD.with(|word| word.load(Ordering::Relaxed))
}

#[doc(hidden)]
pub fn reset_thread_depth_for_test() {
    THREAD_HELD_DEPTH.with(|depth| depth.set(0));
}

#[doc(hidden)]
pub fn reset_lock_id_allocator_for_test() {
    NEXT_LOCK_ID.store(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::{
        IN_CRITICAL_SECTION, MAX_LOCK_CLASSES, SLOW_PATH_PENDING, TOKEN_CONSUMED,
        UNMANAGED_LOCK_ID, USER_ADMISSION_LOCK_ID_SHIFT, allocate_lock_id, begin_lock_scope,
        clear_token_consumed_for_scope, finish_lock_scope, mark_cond_reacquire_pending_for_lock,
        mark_critical_section_entered, mark_critical_section_entered_for_scope,
        mark_critical_section_entered_with_admission_enabled, mark_critical_section_exit,
        mark_critical_section_exit_with_admission_enabled, mark_slow_path_pending,
        mark_slow_path_pending_for_scope, mark_slow_path_pending_with_admission_enabled,
        reset_lock_id_allocator_for_test, reset_state, reset_thread_depth_for_test,
        reset_transient_state, set_inactive_queue_seq_ptrs, slow_path_yield_required,
        token_consumed_for_scope, word_for_test,
    };

    static INACTIVE_QUEUE_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());
    static mut TEST_INACTIVE_ENQUEUE_SEQ: u32 = 0;
    static mut TEST_INACTIVE_EMPTY_SEQ: u32 = 0;

    struct InactiveQueueStateTestGuard {
        _guard: MutexGuard<'static, ()>,
    }

    impl Drop for InactiveQueueStateTestGuard {
        fn drop(&mut self) {
            set_inactive_queue_seq_ptrs(std::ptr::null_mut(), std::ptr::null_mut());
        }
    }

    fn word_for(lock_id: u32, flags: u32) -> u32 {
        (lock_id << USER_ADMISSION_LOCK_ID_SHIFT) | flags
    }

    fn install_inactive_queue_state(
        enqueue_seq: u32,
        empty_seq: u32,
    ) -> InactiveQueueStateTestGuard {
        let guard = INACTIVE_QUEUE_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        unsafe {
            TEST_INACTIVE_ENQUEUE_SEQ = enqueue_seq;
            TEST_INACTIVE_EMPTY_SEQ = empty_seq;
        }
        set_inactive_queue_seq_ptrs(
            &raw mut TEST_INACTIVE_ENQUEUE_SEQ,
            &raw mut TEST_INACTIVE_EMPTY_SEQ,
        );
        InactiveQueueStateTestGuard { _guard: guard }
    }

    #[test]
    fn admission_word_helpers_track_bit_transitions() {
        reset_state();

        mark_slow_path_pending();
        assert_eq!(word_for_test(), SLOW_PATH_PENDING);

        mark_critical_section_entered();
        assert_eq!(word_for_test(), IN_CRITICAL_SECTION);

        mark_slow_path_pending();
        assert_eq!(word_for_test(), IN_CRITICAL_SECTION | SLOW_PATH_PENDING);

        mark_critical_section_exit();
        assert_eq!(word_for_test(), SLOW_PATH_PENDING | TOKEN_CONSUMED);

        mark_critical_section_entered();
        mark_critical_section_exit();
        assert_eq!(word_for_test(), TOKEN_CONSUMED);
    }

    #[test]
    fn measurement_reset_clears_transient_state() {
        reset_state();

        mark_slow_path_pending();
        mark_critical_section_entered();

        reset_transient_state();

        assert_eq!(word_for_test(), 0);

        reset_state();
        assert_eq!(word_for_test(), 0);
    }

    #[test]
    fn disabled_admission_does_not_set_admission_bits() {
        reset_state();

        mark_slow_path_pending_with_admission_enabled(false);
        assert_eq!(word_for_test(), 0);

        mark_critical_section_entered_with_admission_enabled(false);
        assert_eq!(word_for_test(), 0);

        mark_critical_section_exit_with_admission_enabled(false);
        assert_eq!(word_for_test(), 0);
    }

    #[test]
    fn slow_path_pending_reports_whether_admission_policy_is_enabled() {
        reset_state();

        assert!(mark_slow_path_pending_with_admission_enabled(true));
        assert_eq!(word_for_test(), SLOW_PATH_PENDING);

        reset_state();

        assert!(!mark_slow_path_pending_with_admission_enabled(false));
        assert_eq!(word_for_test(), 0);
    }

    #[test]
    fn explicit_lock_scope_encodes_lock_id_and_preserves_it_on_exit() {
        reset_state();
        reset_thread_depth_for_test();

        let scope = begin_lock_scope(3);
        assert!(mark_slow_path_pending_for_scope(scope));
        assert_eq!(word_for_test(), word_for(3, SLOW_PATH_PENDING));

        mark_critical_section_entered_for_scope(scope);
        assert_eq!(word_for_test(), word_for(3, IN_CRITICAL_SECTION));

        finish_lock_scope(3);
        assert_eq!(word_for_test(), word_for(3, TOKEN_CONSUMED));

        reset_state();
        assert_eq!(word_for_test(), 0);
    }

    #[test]
    fn unmanaged_scope_does_not_write_admission_bits() {
        reset_state();
        reset_thread_depth_for_test();

        let scope = begin_lock_scope(UNMANAGED_LOCK_ID);
        assert!(!mark_slow_path_pending_for_scope(scope));
        mark_critical_section_entered_for_scope(scope);
        finish_lock_scope(UNMANAGED_LOCK_ID);

        assert_eq!(word_for_test(), 0);
    }

    #[test]
    fn nested_managed_lock_does_not_replace_outer_lock_id() {
        reset_state();
        reset_thread_depth_for_test();

        let outer = begin_lock_scope(2);
        assert!(mark_slow_path_pending_for_scope(outer));
        mark_critical_section_entered_for_scope(outer);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        let inner = begin_lock_scope(3);
        assert!(!mark_slow_path_pending_for_scope(inner));
        mark_critical_section_entered_for_scope(inner);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(3);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(2);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));
    }

    #[test]
    fn next_slow_path_after_consuming_token_exposes_consumed_flag_until_yield() {
        reset_state();
        reset_thread_depth_for_test();

        let first = begin_lock_scope(4);
        assert!(mark_slow_path_pending_for_scope(first));
        mark_critical_section_entered_for_scope(first);
        finish_lock_scope(4);
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        let second = begin_lock_scope(4);
        assert!(token_consumed_for_scope(second));
        assert!(mark_slow_path_pending_for_scope(second));
        assert_eq!(
            word_for_test(),
            word_for(4, TOKEN_CONSUMED | SLOW_PATH_PENDING)
        );

        clear_token_consumed_for_scope(second);
        assert!(!token_consumed_for_scope(second));
        assert_eq!(word_for_test(), word_for(4, SLOW_PATH_PENDING));

        reset_thread_depth_for_test();
    }

    #[test]
    fn slow_path_for_different_lock_drops_consumed_token() {
        reset_state();
        reset_thread_depth_for_test();

        let first = begin_lock_scope(4);
        assert!(mark_slow_path_pending_for_scope(first));
        mark_critical_section_entered_for_scope(first);
        finish_lock_scope(4);
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        let second = begin_lock_scope(5);
        assert!(!token_consumed_for_scope(second));
        assert!(mark_slow_path_pending_for_scope(second));

        assert_eq!(word_for_test(), word_for(5, SLOW_PATH_PENDING));

        reset_thread_depth_for_test();
    }

    #[test]
    fn cond_reacquire_hint_marks_slow_path_and_consumed_token_for_lock() {
        reset_state();

        mark_cond_reacquire_pending_for_lock(4);

        assert_eq!(
            word_for_test(),
            word_for(4, SLOW_PATH_PENDING | TOKEN_CONSUMED)
        );
    }

    #[test]
    fn slow_path_without_consumed_token_still_yields_to_request_admission() {
        let _queue_state = install_inactive_queue_state(7, 7);

        assert!(slow_path_yield_required(false));
    }

    #[test]
    fn consumed_token_reuse_skips_yield_when_inactive_queue_is_idle() {
        let _queue_state = install_inactive_queue_state(7, 7);

        assert!(!slow_path_yield_required(true));
    }

    #[test]
    fn consumed_token_reuse_yields_when_inactive_queue_has_pending_work() {
        let _queue_state = install_inactive_queue_state(8, 7);

        assert!(slow_path_yield_required(true));
    }

    #[test]
    fn lock_id_allocator_exhausts_at_configured_class_limit() {
        reset_lock_id_allocator_for_test();

        assert_eq!(MAX_LOCK_CLASSES, 16);
        for expected in 1..MAX_LOCK_CLASSES {
            assert_eq!(allocate_lock_id(), expected);
        }

        assert_eq!(allocate_lock_id(), UNMANAGED_LOCK_ID);
        assert_eq!(allocate_lock_id(), UNMANAGED_LOCK_ID);
    }
}
