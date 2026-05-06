use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

const IN_CRITICAL_SECTION: u32 = 1 << 0;
const SLOW_PATH_PENDING: u32 = 1 << 1;
const SLOW_PATH_SEEN: u32 = 1 << 2;
pub const DISABLE_ADMISSION_ENV: &str = "ACCORDIN_DISABLE_ADMISSION";

thread_local! {
    static USER_ADMISSION_WORD: AtomicU32 = const { AtomicU32::new(0) };
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
pub fn mark_slow_path_pending() -> bool {
    mark_slow_path_pending_with_admission_enabled(admission_enabled())
}

#[inline(always)]
fn mark_slow_path_pending_with_admission_enabled(enabled: bool) -> bool {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            value | SLOW_PATH_PENDING | SLOW_PATH_SEEN
        } else {
            (value | SLOW_PATH_SEEN) & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION)
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
            (value | IN_CRITICAL_SECTION) & !SLOW_PATH_PENDING
        } else {
            (value | SLOW_PATH_SEEN) & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION)
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
            value & !IN_CRITICAL_SECTION
        } else {
            (value | SLOW_PATH_SEEN) & !(SLOW_PATH_PENDING | IN_CRITICAL_SECTION)
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
        word.store(value & SLOW_PATH_SEEN, Ordering::Relaxed);
    });
}

#[doc(hidden)]
pub fn word_for_test() -> u32 {
    USER_ADMISSION_WORD.with(|word| word.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::{
        IN_CRITICAL_SECTION, SLOW_PATH_PENDING, SLOW_PATH_SEEN, mark_critical_section_entered,
        mark_critical_section_entered_with_admission_enabled, mark_critical_section_exit,
        mark_critical_section_exit_with_admission_enabled, mark_slow_path_pending,
        mark_slow_path_pending_with_admission_enabled, reset_state, reset_transient_state,
        word_for_test,
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
}
