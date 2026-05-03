use std::sync::atomic::{AtomicU32, Ordering};

const IN_CRITICAL_SECTION: u32 = 1 << 0;
const SLOW_PATH_PENDING: u32 = 1 << 1;

thread_local! {
    static USER_ADMISSION_WORD: AtomicU32 = const { AtomicU32::new(0) };
}

pub fn user_word_addr() -> *const u32 {
    USER_ADMISSION_WORD.with(|word| word as *const AtomicU32 as *const u32)
}

#[inline(always)]
pub fn mark_slow_path_pending() {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(value | SLOW_PATH_PENDING, Ordering::Relaxed);
    });
}

#[inline(always)]
pub fn mark_critical_section_entered() {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(
            (value | IN_CRITICAL_SECTION) & !SLOW_PATH_PENDING,
            Ordering::Relaxed,
        );
    });
}

#[inline(always)]
pub fn mark_critical_section_exit() {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(value & !IN_CRITICAL_SECTION, Ordering::Relaxed);
    });
}

#[inline(always)]
pub fn reset_state() {
    USER_ADMISSION_WORD.with(|word| {
        word.store(0, Ordering::Relaxed);
    });
}

#[doc(hidden)]
pub fn word_for_test() -> u32 {
    USER_ADMISSION_WORD.with(|word| word.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::{
        IN_CRITICAL_SECTION, SLOW_PATH_PENDING, mark_critical_section_entered,
        mark_critical_section_exit, mark_slow_path_pending, reset_state, word_for_test,
    };

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
        assert_eq!(word_for_test(), SLOW_PATH_PENDING);

        mark_critical_section_entered();
        mark_critical_section_exit();
        assert_eq!(word_for_test(), 0);
    }
}
