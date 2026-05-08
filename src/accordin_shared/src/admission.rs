use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

const IN_CRITICAL_SECTION: u64 = 1 << 0;
const SLOW_PATH_PENDING: u64 = 1 << 1;
const SLOW_PATH_SEEN: u64 = 1 << 2;
const LOCK_ID_SHIFT: u32 = 3;
const FLAGS_MASK: u64 = (1 << LOCK_ID_SHIFT) - 1;
pub const DISABLE_ADMISSION_ENV: &str = "ACCORDIN_DISABLE_ADMISSION";

thread_local! {
    static USER_ADMISSION_WORD: AtomicU64 = const { AtomicU64::new(0) };
}

pub fn user_word_addr() -> *const u64 {
    USER_ADMISSION_WORD.with(|word| word as *const AtomicU64 as *const u64)
}

#[inline(always)]
fn admission_enabled() -> bool {
    static ADMISSION_ENABLED: OnceLock<bool> = OnceLock::new();

    *ADMISSION_ENABLED.get_or_init(|| !crate::env::env_flag(DISABLE_ADMISSION_ENV))
}

#[inline(always)]
pub fn mark_slow_path_pending() -> bool {
    mark_slow_path_pending_for_lock(0)
}

#[inline(always)]
pub fn mark_slow_path_pending_for_lock(lock_id: u64) -> bool {
    mark_slow_path_pending_for_lock_with_admission_enabled(lock_id, admission_enabled())
}

#[inline(always)]
#[cfg(test)]
fn mark_slow_path_pending_with_admission_enabled(enabled: bool) -> bool {
    mark_slow_path_pending_for_lock_with_admission_enabled(0, enabled)
}

#[inline(always)]
fn pack_word(flags: u64, lock_id: u64) -> u64 {
    ((lock_id << LOCK_ID_SHIFT) & !FLAGS_MASK) | (flags & FLAGS_MASK)
}

#[inline(always)]
fn word_flags(value: u64) -> u64 {
    value & FLAGS_MASK
}

#[inline(always)]
fn word_lock_id(value: u64) -> u64 {
    value >> LOCK_ID_SHIFT
}

#[inline(always)]
fn mark_slow_path_pending_for_lock_with_admission_enabled(lock_id: u64, enabled: bool) -> bool {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            pack_word(
                word_flags(value) | SLOW_PATH_PENDING | SLOW_PATH_SEEN,
                lock_id,
            )
        } else {
            (word_flags(value) | SLOW_PATH_SEEN) & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION)
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
fn mark_critical_section_entered_with_admission_enabled(enabled: bool) {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            pack_word(
                (word_flags(value) | IN_CRITICAL_SECTION) & !SLOW_PATH_PENDING,
                word_lock_id(value),
            )
        } else {
            (word_flags(value) | SLOW_PATH_SEEN) & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION)
        };
        word.store(next, Ordering::Relaxed);
    });
}

#[inline(always)]
pub fn mark_critical_section_exit() {
    mark_critical_section_exit_with_admission_enabled(admission_enabled());
}

#[inline(always)]
fn mark_critical_section_exit_with_admission_enabled(enabled: bool) {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            word_flags(value) & !IN_CRITICAL_SECTION
        } else {
            (word_flags(value) | SLOW_PATH_SEEN) & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION)
        };
        word.store(next, Ordering::Relaxed);
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
        word.store(word_flags(value) & SLOW_PATH_SEEN, Ordering::Relaxed);
    });
}

#[doc(hidden)]
pub fn word_for_test() -> u64 {
    USER_ADMISSION_WORD.with(|word| word.load(Ordering::Relaxed))
}

#[doc(hidden)]
pub fn requested_lock_id_for_test() -> u64 {
    USER_ADMISSION_WORD.with(|word| word_lock_id(word.load(Ordering::Relaxed)))
}

#[cfg(test)]
mod tests {
    use super::{
        IN_CRITICAL_SECTION, SLOW_PATH_PENDING, SLOW_PATH_SEEN, mark_critical_section_entered,
        mark_critical_section_entered_with_admission_enabled, mark_critical_section_exit,
        mark_critical_section_exit_with_admission_enabled, mark_slow_path_pending,
        mark_slow_path_pending_for_lock, mark_slow_path_pending_with_admission_enabled,
        requested_lock_id_for_test, reset_state, reset_transient_state, word_for_test,
    };

    #[test]
    fn admission_word_helpers_track_bit_transitions() {
        reset_state();

        mark_slow_path_pending();
        assert_eq!(word_for_test(), SLOW_PATH_PENDING | SLOW_PATH_SEEN);

        mark_critical_section_entered();
        assert_eq!(word_for_test(), IN_CRITICAL_SECTION | SLOW_PATH_SEEN);

        mark_slow_path_pending();
        assert_eq!(
            word_for_test(),
            IN_CRITICAL_SECTION | SLOW_PATH_PENDING | SLOW_PATH_SEEN
        );

        mark_critical_section_exit();
        assert_eq!(word_for_test(), SLOW_PATH_PENDING | SLOW_PATH_SEEN);

        mark_critical_section_entered();
        mark_critical_section_exit();
        assert_eq!(word_for_test(), SLOW_PATH_SEEN);
    }

    #[test]
    fn measurement_reset_preserves_slow_path_history() {
        reset_state();

        mark_slow_path_pending();
        mark_critical_section_entered();

        reset_transient_state();

        assert_eq!(word_for_test(), SLOW_PATH_SEEN);

        reset_state();
        assert_eq!(word_for_test(), 0);
    }

    #[test]
    fn disabled_admission_marks_controlled_thread_without_admission_bits() {
        reset_state();

        mark_slow_path_pending_with_admission_enabled(false);
        assert_eq!(word_for_test(), SLOW_PATH_SEEN);

        mark_critical_section_entered_with_admission_enabled(false);
        assert_eq!(word_for_test(), SLOW_PATH_SEEN);

        mark_critical_section_exit_with_admission_enabled(false);
        assert_eq!(word_for_test(), SLOW_PATH_SEEN);
    }

    #[test]
    fn slow_path_pending_reports_whether_admission_policy_is_enabled() {
        reset_state();

        assert!(mark_slow_path_pending_with_admission_enabled(true));
        assert_eq!(word_for_test(), SLOW_PATH_PENDING | SLOW_PATH_SEEN);

        reset_state();

        assert!(!mark_slow_path_pending_with_admission_enabled(false));
        assert_eq!(word_for_test(), SLOW_PATH_SEEN);
    }

    #[test]
    fn slow_path_pending_packs_requested_lock_id_into_admission_word() {
        reset_state();

        assert!(mark_slow_path_pending_for_lock(0x1234_5678));

        assert_eq!(
            word_for_test() & (IN_CRITICAL_SECTION | SLOW_PATH_PENDING | SLOW_PATH_SEEN),
            SLOW_PATH_PENDING | SLOW_PATH_SEEN
        );
        assert_eq!(requested_lock_id_for_test(), 0x1234_5678);
    }

    #[test]
    fn critical_section_enter_preserves_lock_id_and_exit_clears_it() {
        reset_state();

        mark_slow_path_pending_for_lock(0x1234_5678);
        mark_critical_section_entered();

        assert_eq!(
            word_for_test() & (IN_CRITICAL_SECTION | SLOW_PATH_PENDING | SLOW_PATH_SEEN),
            IN_CRITICAL_SECTION | SLOW_PATH_SEEN
        );
        assert_eq!(requested_lock_id_for_test(), 0x1234_5678);

        mark_critical_section_exit();

        assert_eq!(word_for_test(), SLOW_PATH_SEEN);
        assert_eq!(requested_lock_id_for_test(), 0);
    }

    #[test]
    fn transient_reset_preserves_history_but_clears_lock_id() {
        reset_state();

        mark_slow_path_pending_for_lock(0x1234_5678);

        reset_transient_state();

        assert_eq!(word_for_test(), SLOW_PATH_SEEN);
        assert_eq!(requested_lock_id_for_test(), 0);
    }
}
