use std::cell::Cell;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, Ordering};

const IN_CRITICAL_SECTION: u32 = 1 << 0;
const SLOW_PATH_PENDING: u32 = 1 << 1;
const TOKEN_CONSUMED: u32 = 1 << 2;
/// Everything an admission-disabled word update clears: the flags that describe
/// one in-flight acquisition rather than the lock's identity.
const TRANSIENT_FLAGS: u32 = SLOW_PATH_PENDING | IN_CRITICAL_SECTION | TOKEN_CONSUMED;
pub const USER_ADMISSION_FLAG_MASK: u32 = 0x7;
pub const USER_ADMISSION_LOCK_ID_SHIFT: u32 = 3;
/// Must equal `MAX_LOCK_CLASSES` in the BPF `intf.h`: the scheduler loader
/// assigns a `[u32; CLASS_COUNT]` into the skeleton's per-class BSS array, so a
/// mismatch is a build error rather than a silent truncation.
pub const MAX_LOCK_CLASSES: u32 = 64;
pub const UNMANAGED_LOCK_ID: u32 = 0;
pub const DISABLE_ADMISSION_ENV: &str = "ACCORDIN_DISABLE_ADMISSION";
const WIDTH_MERGE_ENV: &str = "ACCORDIN_WIDTH_MERGE";
const DUMP_DEPENDENCY_ENV: &str = "ACCORDIN_DUMP_DEPENDENCY";
const WIDTH_OVERFLOW_POLICY_ENV: &str = "ACCORDIN_WIDTH_OVERFLOW_POLICY";

/// Number of lock classes, i.e. the length of every per-class array on both
/// sides of the BPF boundary. Index 0 is the unmanaged sentinel.
pub const CLASS_COUNT: usize = MAX_LOCK_CLASSES as usize;

static NEXT_LOCK_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_OVERFLOW_INDEX: AtomicU32 = AtomicU32::new(0);
static INACTIVE_ENQUEUE_SEQ_PTR: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
static INACTIVE_EMPTY_SEQ_PTR: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());

static LOCK_CLASS_PARENT: [AtomicU8; CLASS_COUNT] = identity_lock_classes();
static LOCK_CLASS_MERGE_LOCK: Mutex<()> = Mutex::new(());
static CLASSES_DIRTY: AtomicBool = AtomicBool::new(false);
static TRANSITION_COUNTS: [[AtomicU32; CLASS_COUNT]; CLASS_COUNT] =
    [const { [const { AtomicU32::new(0) }; CLASS_COUNT] }; CLASS_COUNT];

thread_local! {
    static USER_ADMISSION_WORD: AtomicU32 = const { AtomicU32::new(0) };
    static THREAD_HELD_DEPTH: Cell<u32> = const { Cell::new(0) };
    static THREAD_OUTER_LOCK_ID: Cell<u32> = const { Cell::new(UNMANAGED_LOCK_ID) };
    static THREAD_PREV_OUTERMOST_CLASS: Cell<u32> = const { Cell::new(UNMANAGED_LOCK_ID) };
}

#[cfg(test)]
thread_local! {
    static MERGE_FORCED: Cell<bool> = const { Cell::new(false) };
    static DUMP_FORCED: Cell<bool> = const { Cell::new(false) };
    static LOCK_CLASS_POLICY_FORCED: Cell<Option<LockClassPolicy>> = const { Cell::new(None) };
}

/// What a lock created past the class limit receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockClassPolicy {
    /// An id shared with an earlier lock, exactly as a merged lock has.
    Fold,
    /// No id, so the lock stays outside admission control.
    Unmanaged,
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

const fn identity_lock_classes() -> [AtomicU8; CLASS_COUNT] {
    let mut parents = [const { AtomicU8::new(0) }; CLASS_COUNT];
    let mut id = 0;
    while id < CLASS_COUNT {
        parents[id] = AtomicU8::new(id as u8);
        id += 1;
    }
    parents
}

#[cfg(test)]
#[inline(always)]
fn merge_forced() -> bool {
    MERGE_FORCED.with(|forced| forced.get())
}

#[cfg(not(test))]
#[inline(always)]
fn merge_forced() -> bool {
    false
}

#[cfg(test)]
#[inline(always)]
fn dump_forced() -> bool {
    DUMP_FORCED.with(|forced| forced.get())
}

#[cfg(not(test))]
#[inline(always)]
fn dump_forced() -> bool {
    false
}

#[cfg(test)]
#[inline(always)]
fn forced_lock_class_policy() -> Option<LockClassPolicy> {
    LOCK_CLASS_POLICY_FORCED.with(|policy| policy.get())
}

#[cfg(not(test))]
#[inline(always)]
fn forced_lock_class_policy() -> Option<LockClassPolicy> {
    None
}

/// Anything other than `unmanaged` keeps the `fold` default, so a typo in the
/// environment does not silently drop locks out of admission control.
fn parse_lock_class_policy(value: Option<&str>) -> LockClassPolicy {
    match value.map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("unmanaged") => LockClassPolicy::Unmanaged,
        _ => LockClassPolicy::Fold,
    }
}

/// The overflow policy is a width-control policy: with the feature off there is
/// no width to share, so allocation stops at the class limit as it does without
/// any of this machinery.
fn lock_class_policy() -> LockClassPolicy {
    if let Some(forced) = forced_lock_class_policy() {
        return forced;
    }

    static POLICY: OnceLock<LockClassPolicy> = OnceLock::new();

    *POLICY.get_or_init(|| {
        if !crate::width_control::enabled() {
            return LockClassPolicy::Unmanaged;
        }

        parse_lock_class_policy(std::env::var(WIDTH_OVERFLOW_POLICY_ENV).ok().as_deref())
    })
}

/// Dependency merging and a statically configured width are mutually exclusive,
/// and the static configuration wins even against an explicit merge request.
/// A merge renames a lock's class, so an entry naming that class in the
/// operator's map stops describing anything the lock resolves to; and the fold
/// that reconciles a merged-away class with its canonical one belongs to the
/// controller tick, which a static width configuration replaces. Leaving the
/// two on together would let a merge silently retire a configured width with no
/// signal that it stopped applying.
fn merge_enabled_with(width_control: bool, fixed_widths: bool, requested: bool) -> bool {
    width_control && !fixed_widths && requested
}

#[inline(always)]
fn merge_enabled() -> bool {
    if merge_forced() {
        return true;
    }

    static MERGE_ENABLED: OnceLock<bool> = OnceLock::new();

    *MERGE_ENABLED.get_or_init(|| {
        merge_enabled_with(
            crate::width_control::enabled(),
            crate::width_control::fixed_widths_configured(),
            crate::env::env_flag_default_on(WIDTH_MERGE_ENV),
        )
    })
}

#[inline(always)]
fn dump_dependency_enabled() -> bool {
    if dump_forced() {
        return true;
    }

    static DUMP_DEPENDENCY: OnceLock<bool> = OnceLock::new();

    *DUMP_DEPENDENCY.get_or_init(|| crate::env::env_flag(DUMP_DEPENDENCY_ENV))
}

/// Walks parent pointers to the component root. A merge only ever points a root
/// at a strictly smaller id and never re-parents an entry afterwards, so the
/// walk is strictly decreasing and terminates even if a merge lands mid-walk.
#[inline(always)]
fn find_root(lock_id: u32) -> u32 {
    let mut root = lock_id;
    loop {
        let parent = LOCK_CLASS_PARENT[root as usize].load(Ordering::Relaxed) as u32;
        if parent == root {
            return root;
        }
        root = parent;
    }
}

fn union_lock_classes(a: u32, b: u32) {
    if !managed_lock_id(a) || !managed_lock_id(b) {
        return;
    }

    let _guard = LOCK_CLASS_MERGE_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let root_a = find_root(a);
    let root_b = find_root(b);
    if root_a == root_b {
        return;
    }

    let (min_root, max_root) = if root_a < root_b {
        (root_a, root_b)
    } else {
        (root_b, root_a)
    };
    LOCK_CLASS_PARENT[max_root as usize].store(min_root as u8, Ordering::Relaxed);
    CLASSES_DIRTY.store(true, Ordering::Relaxed);
}

/// Scheduling class of a lock: the smallest lock id of its merged component.
///
/// This is the id published in the admission word, so BPF reads a merged
/// component as one identity: one shared inactive DSQ and one shared width,
/// with no mapping table of its own. A thread that still carries the id from
/// before a merge is harmless — that id is a valid class whose DSQ drains
/// normally, and the admission counters balance because BPF decrements the
/// class it recorded at grant time.
#[inline(always)]
pub fn class_of(lock_id: u32) -> u32 {
    if !managed_lock_id(lock_id) || !merge_enabled() {
        return lock_id;
    }

    find_root(lock_id)
}

/// Reports (and clears) whether classes were merged since the last call.
pub fn take_classes_dirty() -> bool {
    CLASSES_DIRTY.swap(false, Ordering::Relaxed)
}

#[inline(always)]
fn record_nesting_edge(inner: u32) {
    if !merge_enabled() {
        return;
    }

    let outer = THREAD_OUTER_LOCK_ID.with(|outer| outer.get());
    if outer == inner || !managed_lock_id(outer) {
        return;
    }

    // Lock-free reads first: only a genuinely new edge takes the merge mutex.
    if find_root(outer) == find_root(inner) {
        return;
    }

    union_lock_classes(outer, inner);
}

/// Counts sequential handoffs between outermost classes. Diagnostics only: a
/// transition is not a nesting edge and never merges classes.
#[inline(always)]
fn record_outermost_transition(lock_id: u32) {
    if !dump_dependency_enabled() {
        return;
    }

    let next = class_of(lock_id);
    if !managed_lock_id(next) {
        return;
    }

    let prev = THREAD_PREV_OUTERMOST_CLASS.with(|prev| prev.replace(next));
    if managed_lock_id(prev) {
        TRANSITION_COUNTS[prev as usize][next as usize].fetch_add(1, Ordering::Relaxed);
    }
}

pub fn dump_dependency_diagnostics() {
    if !dump_dependency_enabled() {
        return;
    }

    for lock_id in 1..MAX_LOCK_CLASSES {
        let class = find_root(lock_id);
        if class != lock_id {
            eprintln!("dependency_merge: lock={lock_id} class={class}");
        }
    }

    for (prev, row) in TRANSITION_COUNTS.iter().enumerate().skip(1) {
        for (next, cell) in row.iter().enumerate().skip(1) {
            let count = cell.load(Ordering::Relaxed);
            if count != 0 {
                eprintln!("dependency_transition: from={prev} to={next} count={count}");
            }
        }
    }
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

/// Word published while admission is disabled: the lock class stays visible and
/// every admission flag is dropped.
#[inline(always)]
fn cleared_flags_word(class: u32, value: u32) -> u32 {
    word_with_lock_id(class, flags(value) & !TRANSIENT_FLAGS)
}

#[inline(always)]
fn exit_word(class: u32, value: u32, enabled: bool) -> u32 {
    if enabled {
        word_with_lock_id(class, (flags(value) | TOKEN_CONSUMED) & !IN_CRITICAL_SECTION)
    } else {
        cleared_flags_word(class, value)
    }
}

/// Hands out one id per lock while ids last, then reports every further lock as
/// unmanaged.
fn allocate_lock_id() -> u32 {
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

/// Scheduling class a lock is created with. The first `MAX_LOCK_CLASSES - 1`
/// locks each get an id to themselves; from there the overflow policy decides
/// whether the lock shares a class or drops out of admission control.
///
/// The id is drawn once, at creation, and the lock carries it unchanged for its
/// whole life: nothing here maps a lock back to an id, so no later allocation
/// can renumber a lock that already exists.
pub fn allocate_lock_class() -> u32 {
    let lock_id = allocate_lock_id();
    // The probe span follows the ids handed out. Publishing it here rather
    // than from the controller tick covers the fixed-width mode, where the
    // tick never runs.
    crate::width_control::publish_inactive_probe_span();
    if lock_id != UNMANAGED_LOCK_ID {
        return lock_id;
    }

    match lock_class_policy() {
        LockClassPolicy::Fold => fold_overflow_lock_id(),
        LockClassPolicy::Unmanaged => UNMANAGED_LOCK_ID,
    }
}

/// Managed lock ids handed out so far. Ids are dense from 1 upward and an
/// overflowing lock folds onto an id already in use, so this is also the
/// highest managed id in play.
pub fn allocated_class_count() -> u32 {
    NEXT_LOCK_ID.load(Ordering::Relaxed).saturating_sub(1)
}

/// Deals overflowing locks round-robin over the managed ids. Folding reuses the
/// merge mechanism rather than adding one: the lock shares its id, and with it
/// one inactive DSQ and one width, with an earlier lock, so `class_of` and
/// everything downstream of it treat the two as the single class they are.
fn fold_overflow_lock_id() -> u32 {
    let overflow_index = NEXT_OVERFLOW_INDEX.fetch_add(1, Ordering::Relaxed);
    1 + overflow_index % (MAX_LOCK_CLASSES - 1)
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

        let managed = managed_lock_id(lock_id);
        if held == 0 {
            THREAD_OUTER_LOCK_ID.with(|outer| outer.set(lock_id));
            record_outermost_transition(lock_id);
        } else if managed {
            record_nesting_edge(lock_id);
        }

        LockAdmissionScope {
            lock_id,
            outer_managed: held == 0 && managed,
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

/// Publishes the critical-section exit for the lock that currently owns the
/// admission word, even when an inner scope is still held: condition variables
/// release the outer contended mutex while their internal mutex stays locked,
/// and leaving `IN_CRITICAL_SECTION` set there pins the CPU admission slot for
/// the whole sleep.
#[inline(always)]
pub fn finish_lock_scope(lock_id: u32) {
    THREAD_HELD_DEPTH.with(|depth| {
        let held = depth.get();
        if held == 0 {
            return;
        }

        if managed_lock_id(lock_id) {
            if held == 1 {
                mark_critical_section_exit_for_lock(lock_id);
            } else {
                mark_critical_section_exit_for_owned_lock(lock_id);
            }
        }
        if held == 1 {
            THREAD_OUTER_LOCK_ID.with(|outer| outer.set(UNMANAGED_LOCK_ID));
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
pub fn mark_cond_reacquire_pending_for_cond_mutex(lock_id: u32) -> bool {
    mark_cond_reacquire_pending_for_cond_mutex_with_admission_enabled(lock_id, admission_enabled())
}

/// Acknowledges a cond-reacquire hint published for this scope: the consumed
/// token is cleared in the same word access that matched it, so the caller can
/// block on the lock without asking admission again.
#[inline(always)]
pub fn take_cond_reacquire_pending_for_scope(scope: LockAdmissionScope) -> bool {
    if !scope.outer_managed {
        return false;
    }

    let class = class_of(scope.lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        if user_lock_id_for_value(value) != class
            || flags(value) & (SLOW_PATH_PENDING | TOKEN_CONSUMED)
                != SLOW_PATH_PENDING | TOKEN_CONSUMED
        {
            return false;
        }

        word.store(
            word_with_lock_id(class, flags(value) & !TOKEN_CONSUMED),
            Ordering::Relaxed,
        );
        true
    })
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

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        user_lock_id_for_value(value) == class && flags(value) & TOKEN_CONSUMED != 0
    })
}

#[inline(always)]
fn mark_slow_path_pending_with_admission_enabled(enabled: bool) -> bool {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            value | SLOW_PATH_PENDING
        } else {
            value & !TRANSIENT_FLAGS
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

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            let preserved_flags = if user_lock_id_for_value(value) == class {
                flags(value) & TOKEN_CONSUMED
            } else {
                0
            };
            word_with_lock_id(class, preserved_flags | SLOW_PATH_PENDING)
        } else {
            cleared_flags_word(class, value)
        };
        word.store(next, Ordering::Relaxed);
    });
    enabled
}

/// Publishes the cond-reacquire hint only while the word still describes the
/// just-unlocked cond mutex and no managed scope stays held: any other state
/// belongs to an enclosing critical section that must keep owning the word.
#[inline(always)]
fn mark_cond_reacquire_pending_for_cond_mutex_with_admission_enabled(
    lock_id: u32,
    enabled: bool,
) -> bool {
    if !managed_lock_id(lock_id) || THREAD_HELD_DEPTH.with(|depth| depth.get()) != 0 {
        return false;
    }

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        if user_lock_id_for_value(value) != class {
            return false;
        }

        let next = if enabled {
            word_with_lock_id(
                class,
                (flags(value) | SLOW_PATH_PENDING | TOKEN_CONSUMED) & !IN_CRITICAL_SECTION,
            )
        } else {
            cleared_flags_word(class, value)
        };
        word.store(next, Ordering::Relaxed);
        enabled
    })
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
            value & !TRANSIENT_FLAGS
        };
        word.store(next, Ordering::Relaxed);
    });
}

#[inline(always)]
fn mark_critical_section_entered_for_lock_with_admission_enabled(lock_id: u32, enabled: bool) {
    if !managed_lock_id(lock_id) {
        return;
    }

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            word_with_lock_id(
                class,
                (flags(value) | IN_CRITICAL_SECTION) & !(SLOW_PATH_PENDING | TOKEN_CONSUMED),
            )
        } else {
            cleared_flags_word(class, value)
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
            value & !TRANSIENT_FLAGS
        };
        word.store(next, Ordering::Relaxed);
    });
}

#[inline(always)]
fn mark_critical_section_exit_for_lock_with_admission_enabled(lock_id: u32, enabled: bool) {
    if !managed_lock_id(lock_id) {
        return;
    }

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(exit_word(class, value, enabled), Ordering::Relaxed);
    });
}

/// Out-of-order unlock: the exit word is published only for the lock that owns
/// the admission word, which is the outermost one this thread entered.
///
/// Ownership is decided on lock identity rather than on the class in the word,
/// because a merge makes an inner lock share the outer lock's class: comparing
/// classes would accept an ordinary nested release as if it were the outer
/// one and drop `IN_CRITICAL_SECTION` while the outer lock is still held.
#[inline(always)]
fn mark_critical_section_exit_for_owned_lock(lock_id: u32) {
    if THREAD_OUTER_LOCK_ID.with(|outer| outer.get()) != lock_id {
        return;
    }

    let enabled = admission_enabled();
    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(exit_word(class, value, enabled), Ordering::Relaxed);
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

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(
            word_with_lock_id(class, flags(value) & !TOKEN_CONSUMED),
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
    THREAD_OUTER_LOCK_ID.with(|outer| outer.set(UNMANAGED_LOCK_ID));
}

#[doc(hidden)]
pub fn reset_lock_id_allocator_for_test() {
    NEXT_LOCK_ID.store(1, Ordering::Relaxed);
    NEXT_OVERFLOW_INDEX.store(0, Ordering::Relaxed);
}

/// Merging and dump gating are forced per thread so that a test never changes
/// what concurrently running tests observe.
#[cfg(test)]
fn force_merge_for_test(forced: bool) {
    MERGE_FORCED.with(|merge| merge.set(forced));
}

#[cfg(test)]
fn force_dump_for_test(forced: bool) {
    DUMP_FORCED.with(|dump| dump.set(forced));
}

#[cfg(test)]
fn force_lock_class_policy_for_test(forced: Option<LockClassPolicy>) {
    LOCK_CLASS_POLICY_FORCED.with(|policy| policy.set(forced));
}

#[cfg(test)]
fn reset_union_find_for_tests() {
    for (lock_id, parent) in LOCK_CLASS_PARENT.iter().enumerate() {
        parent.store(lock_id as u8, Ordering::Relaxed);
    }
    CLASSES_DIRTY.store(false, Ordering::Relaxed);
}

#[cfg(test)]
fn reset_transitions_for_tests() {
    for row in TRANSITION_COUNTS.iter() {
        for cell in row.iter() {
            cell.store(0, Ordering::Relaxed);
        }
    }
    THREAD_PREV_OUTERMOST_CLASS.with(|prev| prev.set(UNMANAGED_LOCK_ID));
}

#[cfg(test)]
fn transition_count_for_test(prev: u32, next: u32) -> u32 {
    TRANSITION_COUNTS[prev as usize][next as usize].load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::{
        IN_CRITICAL_SECTION, LockClassPolicy, MAX_LOCK_CLASSES, SLOW_PATH_PENDING, TOKEN_CONSUMED,
        UNMANAGED_LOCK_ID, USER_ADMISSION_LOCK_ID_SHIFT, allocate_lock_class, allocate_lock_id,
        allocated_class_count, begin_lock_scope, class_of, clear_token_consumed_for_scope,
        finish_lock_scope, force_dump_for_test, force_lock_class_policy_for_test,
        force_merge_for_test, lock_class_policy, managed_lock_id,
        mark_cond_reacquire_pending_for_cond_mutex,
        mark_cond_reacquire_pending_for_cond_mutex_with_admission_enabled,
        mark_critical_section_entered, mark_critical_section_entered_for_scope,
        mark_critical_section_entered_with_admission_enabled, mark_critical_section_exit,
        mark_critical_section_exit_with_admission_enabled, mark_slow_path_pending,
        mark_slow_path_pending_for_scope, mark_slow_path_pending_with_admission_enabled,
        merge_enabled_with, parse_lock_class_policy, reset_lock_id_allocator_for_test, reset_state,
        reset_thread_depth_for_test, reset_transient_state, reset_transitions_for_tests,
        reset_union_find_for_tests, set_inactive_queue_seq_ptrs, slow_path_yield_required,
        take_classes_dirty, take_cond_reacquire_pending_for_scope, token_consumed_for_scope,
        transition_count_for_test, union_lock_classes, word_for_test,
    };

    static INACTIVE_QUEUE_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());
    static DEPENDENCY_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());
    static ALLOCATOR_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());
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

    struct DependencyStateTestGuard {
        _guard: MutexGuard<'static, ()>,
    }

    impl Drop for DependencyStateTestGuard {
        fn drop(&mut self) {
            force_merge_for_test(false);
            force_dump_for_test(false);
            reset_union_find_for_tests();
            reset_transitions_for_tests();
            reset_thread_depth_for_test();
        }
    }

    struct AllocatorStateTestGuard {
        _guard: MutexGuard<'static, ()>,
    }

    impl Drop for AllocatorStateTestGuard {
        fn drop(&mut self) {
            force_lock_class_policy_for_test(None);
            reset_lock_id_allocator_for_test();
        }
    }

    /// Serializes the tests that draw from the process-wide id allocator and
    /// starts them from a known state. The policy is forced on this thread
    /// only, so tests running in parallel still see the configured one.
    fn isolate_allocator_state(policy: Option<LockClassPolicy>) -> AllocatorStateTestGuard {
        let guard = ALLOCATOR_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        reset_lock_id_allocator_for_test();
        force_lock_class_policy_for_test(policy);
        AllocatorStateTestGuard { _guard: guard }
    }

    fn allocate_lock_classes(count: usize) -> Vec<u32> {
        (0..count).map(|_| allocate_lock_class()).collect()
    }

    fn classes_of(lock_ids: &[u32]) -> Vec<u32> {
        lock_ids.iter().map(|lock_id| class_of(*lock_id)).collect()
    }

    /// Serializes the tests that mutate the process-wide union-find and
    /// transition matrix, and starts them from a known state. Merging is forced
    /// on this thread only, so tests running in parallel still see it off.
    fn isolate_dependency_state() -> DependencyStateTestGuard {
        let guard = DEPENDENCY_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        reset_union_find_for_tests();
        reset_transitions_for_tests();
        reset_thread_depth_for_test();
        force_merge_for_test(true);
        force_dump_for_test(false);
        DependencyStateTestGuard { _guard: guard }
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

    /// Nesting merges the inner lock into the outer lock's class, so the class
    /// the word carries stops distinguishing the two. Releasing the inner lock
    /// must still leave the outer critical section published: dropping
    /// `IN_CRITICAL_SECTION` here lets the scheduler reclaim the admission slot
    /// while the outer lock is held.
    #[test]
    fn a_merged_inner_unlock_keeps_the_outer_critical_section_published() {
        let _guard = isolate_dependency_state();
        reset_state();

        let outer = begin_lock_scope(2);
        assert!(mark_slow_path_pending_for_scope(outer));
        mark_critical_section_entered_for_scope(outer);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        let inner = begin_lock_scope(3);
        assert_eq!(class_of(3), 2, "nesting merges the inner lock");
        assert!(!mark_slow_path_pending_for_scope(inner));
        mark_critical_section_entered_for_scope(inner);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(3);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(2);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));
    }

    /// The out-of-order release the owned-lock branch exists for still has to
    /// work once the two locks share a class.
    #[test]
    fn a_merged_out_of_order_unlock_still_publishes_the_outer_exit_word() {
        let _guard = isolate_dependency_state();
        reset_state();

        let outer = begin_lock_scope(2);
        mark_critical_section_entered_for_scope(outer);

        let inner = begin_lock_scope(3);
        assert_eq!(class_of(3), 2, "nesting merges the inner lock");
        assert!(!mark_slow_path_pending_for_scope(inner));

        finish_lock_scope(2);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));

        finish_lock_scope(3);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));
    }

    #[test]
    fn out_of_order_unlock_publishes_the_exit_word_for_the_outer_lock() {
        reset_state();
        reset_thread_depth_for_test();

        let outer = begin_lock_scope(2);
        assert!(mark_slow_path_pending_for_scope(outer));
        mark_critical_section_entered_for_scope(outer);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        let inner = begin_lock_scope(MAX_LOCK_CLASSES);
        assert!(!mark_slow_path_pending_for_scope(inner));
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(2);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));

        finish_lock_scope(MAX_LOCK_CLASSES);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));

        reset_thread_depth_for_test();
    }

    #[test]
    fn out_of_order_unlock_with_a_managed_inner_lock_publishes_both_exits() {
        reset_state();
        reset_thread_depth_for_test();

        let outer = begin_lock_scope(2);
        mark_critical_section_entered_for_scope(outer);

        let inner = begin_lock_scope(3);
        assert!(!mark_slow_path_pending_for_scope(inner));

        finish_lock_scope(2);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));

        finish_lock_scope(3);
        assert_eq!(word_for_test(), word_for(3, TOKEN_CONSUMED));

        reset_thread_depth_for_test();
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

    fn release_managed_lock_for_test(lock_id: u32) {
        let scope = begin_lock_scope(lock_id);
        mark_critical_section_entered_for_scope(scope);
        finish_lock_scope(lock_id);
    }

    #[test]
    fn cond_reacquire_hint_drives_the_full_relock_transition() {
        reset_state();
        reset_thread_depth_for_test();

        release_managed_lock_for_test(4);
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        assert!(mark_cond_reacquire_pending_for_cond_mutex(4));
        assert_eq!(
            word_for_test(),
            word_for(4, SLOW_PATH_PENDING | TOKEN_CONSUMED)
        );

        let relock = begin_lock_scope(4);
        assert!(take_cond_reacquire_pending_for_scope(relock));
        assert_eq!(word_for_test(), word_for(4, SLOW_PATH_PENDING));

        assert!(!take_cond_reacquire_pending_for_scope(relock));
        assert_eq!(word_for_test(), word_for(4, SLOW_PATH_PENDING));

        mark_critical_section_entered_for_scope(relock);
        assert_eq!(word_for_test(), word_for(4, IN_CRITICAL_SECTION));

        reset_thread_depth_for_test();
    }

    #[test]
    fn cond_reacquire_hint_is_skipped_for_another_lock_id() {
        reset_state();
        reset_thread_depth_for_test();

        release_managed_lock_for_test(4);

        assert!(!mark_cond_reacquire_pending_for_cond_mutex(5));
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        let other = begin_lock_scope(5);
        assert!(!take_cond_reacquire_pending_for_scope(other));
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        reset_thread_depth_for_test();
    }

    #[test]
    fn cond_reacquire_hint_is_skipped_while_an_outer_lock_stays_held() {
        reset_state();
        reset_thread_depth_for_test();

        let outer = begin_lock_scope(2);
        mark_critical_section_entered_for_scope(outer);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        assert!(!mark_cond_reacquire_pending_for_cond_mutex(3));
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(2);
        reset_thread_depth_for_test();
    }

    #[test]
    fn disabled_admission_cond_reacquire_hint_leaves_flags_clear() {
        reset_state();
        reset_thread_depth_for_test();

        release_managed_lock_for_test(4);

        assert!(!mark_cond_reacquire_pending_for_cond_mutex_with_admission_enabled(4, false));
        assert_eq!(word_for_test(), word_for(4, 0));

        reset_thread_depth_for_test();
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
        let _allocator = isolate_allocator_state(None);

        assert_eq!(MAX_LOCK_CLASSES, 64);
        for expected in 1..MAX_LOCK_CLASSES {
            assert_eq!(allocate_lock_id(), expected);
        }

        assert_eq!(allocate_lock_id(), UNMANAGED_LOCK_ID);
        assert_eq!(allocate_lock_id(), UNMANAGED_LOCK_ID);
    }

    #[test]
    fn overflow_policy_folds_unless_the_environment_asks_otherwise() {
        assert_eq!(parse_lock_class_policy(None), LockClassPolicy::Fold);
        assert_eq!(parse_lock_class_policy(Some("fold")), LockClassPolicy::Fold);
        assert_eq!(
            parse_lock_class_policy(Some(" UNMANAGED ")),
            LockClassPolicy::Unmanaged
        );
        // An unrecognized value keeps the default rather than dropping locks.
        assert_eq!(
            parse_lock_class_policy(Some("no-such-policy")),
            LockClassPolicy::Fold
        );
    }

    #[test]
    fn allocation_stops_at_the_class_limit_while_width_control_is_off() {
        let _allocator = isolate_allocator_state(None);

        // With no override the policy comes from the environment, and width
        // control is off in the test process.
        assert_eq!(lock_class_policy(), LockClassPolicy::Unmanaged);

        for expected in 1..MAX_LOCK_CLASSES {
            assert_eq!(allocate_lock_class(), expected);
        }

        assert_eq!(allocate_lock_class(), UNMANAGED_LOCK_ID);
        assert_eq!(allocate_lock_class(), UNMANAGED_LOCK_ID);
    }

    #[test]
    fn unmanaged_policy_leaves_overflowing_locks_outside_admission() {
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Unmanaged));

        let managed: Vec<u32> = (1..MAX_LOCK_CLASSES).collect();
        let ids = allocate_lock_classes(managed.len() + 4);

        assert_eq!(&ids[..managed.len()], &managed[..]);
        assert!(
            ids[managed.len()..]
                .iter()
                .all(|id| *id == UNMANAGED_LOCK_ID)
        );
    }

    #[test]
    fn fold_policy_deals_overflowing_locks_round_robin_over_the_managed_ids() {
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Fold));

        let managed: Vec<u32> = (1..MAX_LOCK_CLASSES).collect();
        let ids = allocate_lock_classes(3 * managed.len());

        assert_eq!(&ids[..managed.len()], &managed[..]);
        // The first locks past the limit fold onto the lowest ids in order.
        assert_eq!(&ids[managed.len()..managed.len() + 5], &[1, 2, 3, 4, 5]);
        // A full round of overflow lands on every managed id exactly once.
        assert_eq!(&ids[managed.len()..2 * managed.len()], &managed[..]);
        assert_eq!(&ids[2 * managed.len()..], &managed[..]);
    }

    #[test]
    fn allocated_class_count_tracks_the_ids_handed_out() {
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Fold));

        assert_eq!(allocated_class_count(), 0);
        allocate_lock_classes(2);
        assert_eq!(allocated_class_count(), 2);

        // Ids run out at the class limit and folded overflow locks reuse them,
        // so the count stops at the managed space.
        allocate_lock_classes(MAX_LOCK_CLASSES as usize + 8);
        assert_eq!(allocated_class_count(), MAX_LOCK_CLASSES - 1);
    }

    #[test]
    fn fold_mapping_repeats_identically_from_a_fresh_allocator() {
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Fold));

        let first = allocate_lock_classes(40);
        reset_lock_id_allocator_for_test();
        let second = allocate_lock_classes(40);

        assert_eq!(first, second);
    }

    #[test]
    fn an_allocated_lock_class_does_not_move_as_more_locks_appear() {
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Fold));

        let early = allocate_lock_classes(MAX_LOCK_CLASSES as usize + 3);
        let early_classes = classes_of(&early);

        allocate_lock_classes(40);

        assert_eq!(classes_of(&early), early_classes);
        assert!(early.iter().all(|lock_id| managed_lock_id(*lock_id)));
    }

    #[test]
    fn folded_locks_resolve_through_the_class_of_the_id_they_share() {
        let _dependency = isolate_dependency_state();
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Fold));

        let managed: Vec<u32> = (1..MAX_LOCK_CLASSES).collect();
        let ids = allocate_lock_classes(managed.len() + 4);
        let folded_onto_three = ids[managed.len() + 2];
        let folded_onto_four = ids[managed.len() + 3];
        assert_eq!(folded_onto_three, 3);
        assert_eq!(folded_onto_four, 4);

        // A folded lock is one of its class, so a merge of that class carries
        // it along with the lock it folded onto.
        union_lock_classes(4, 3);

        for lock_id in [3, 4, folded_onto_three, folded_onto_four] {
            assert_eq!(class_of(lock_id), 3, "lock {lock_id}");
        }

        // Later overflow does not disturb the classes already resolved.
        allocate_lock_classes(managed.len());
        for lock_id in [3, 4, folded_onto_three, folded_onto_four] {
            assert_eq!(class_of(lock_id), 3, "lock {lock_id}");
        }
    }

    #[test]
    fn merged_component_is_named_by_its_smallest_lock_id() {
        let _state = isolate_dependency_state();

        union_lock_classes(5, 2);
        assert_eq!(class_of(5), 2);
        assert_eq!(class_of(2), 2);

        union_lock_classes(11, 7);
        assert_eq!(class_of(7), 7);
        assert_eq!(class_of(11), 7);
    }

    #[test]
    fn merges_are_sticky_and_reported_once_as_dirty() {
        let _state = isolate_dependency_state();

        assert!(!take_classes_dirty());

        union_lock_classes(2, 3);
        assert!(take_classes_dirty());
        assert!(!take_classes_dirty());

        union_lock_classes(3, 5);
        assert!(take_classes_dirty());
        assert_eq!(class_of(2), 2);
        assert_eq!(class_of(3), 2);
        assert_eq!(class_of(5), 2);

        union_lock_classes(5, 2);
        assert!(!take_classes_dirty());
        assert_eq!(class_of(5), 2);
    }

    #[test]
    fn find_converges_through_chained_parent_pointers() {
        let _state = isolate_dependency_state();

        union_lock_classes(15, 14);
        union_lock_classes(14, 12);
        union_lock_classes(12, 3);

        for lock_id in [3, 12, 14, 15] {
            assert_eq!(class_of(lock_id), 3, "lock {lock_id}");
        }
    }

    #[test]
    fn class_of_is_identity_for_unmanaged_and_out_of_range_ids() {
        let _state = isolate_dependency_state();

        assert_eq!(class_of(UNMANAGED_LOCK_ID), UNMANAGED_LOCK_ID);
        assert_eq!(class_of(MAX_LOCK_CLASSES), MAX_LOCK_CLASSES);
        assert_eq!(class_of(99), 99);
    }

    #[test]
    fn class_of_is_identity_while_merging_is_disabled() {
        let _state = isolate_dependency_state();

        union_lock_classes(2, 3);
        assert_eq!(class_of(3), 2);

        force_merge_for_test(false);
        assert_eq!(class_of(3), 3);

        force_merge_for_test(true);
        assert_eq!(class_of(3), 2);
    }

    /// Resolves the merge gate from raw width settings the way the process
    /// resolves it from the environment. Merging is requested throughout, which
    /// is what an explicit `ACCORDIN_WIDTH_MERGE=1` amounts to.
    fn merge_enabled_for_settings(fixed: Option<&str>, class_map: Option<&str>) -> bool {
        const WIDTH_CONTROL: bool = true;
        const REQUESTED: bool = true;

        merge_enabled_with(
            WIDTH_CONTROL,
            crate::width_control::fixed_widths_present_with(WIDTH_CONTROL, fixed, class_map),
            REQUESTED,
        )
    }

    #[test]
    fn a_fixed_width_takes_class_merging_out_of_play() {
        assert!(!merge_enabled_for_settings(Some("4"), None));
    }

    #[test]
    fn a_width_class_map_takes_class_merging_out_of_play() {
        assert!(!merge_enabled_for_settings(None, Some("3:5")));
        assert!(!merge_enabled_for_settings(Some("4"), Some("3:5")));
    }

    #[test]
    fn merging_follows_the_request_while_no_static_width_is_configured() {
        assert!(merge_enabled_for_settings(None, None));
        assert!(merge_enabled_with(true, false, true));
        assert!(!merge_enabled_with(true, false, false));
    }

    #[test]
    fn static_widths_do_not_reach_merging_while_width_control_is_off() {
        // Without the feature there is no width to configure, so neither
        // variable counts as one ...
        assert!(!crate::width_control::fixed_widths_present_with(
            false,
            Some("4"),
            Some("3:5")
        ));
        // ... and merging is off either way, exactly as it is without any of
        // this machinery.
        assert!(!merge_enabled_with(false, false, true));
        assert!(!merge_enabled_with(false, true, true));
    }

    #[test]
    fn only_nested_acquires_of_distinct_managed_locks_merge_classes() {
        let _state = isolate_dependency_state();

        begin_lock_scope(2);
        begin_lock_scope(3);
        finish_lock_scope(3);
        finish_lock_scope(2);
        assert_eq!(class_of(3), 2);
        assert_eq!(class_of(2), 2);

        begin_lock_scope(6);
        finish_lock_scope(6);
        begin_lock_scope(7);
        finish_lock_scope(7);
        assert_eq!(class_of(6), 6);
        assert_eq!(class_of(7), 7);

        begin_lock_scope(UNMANAGED_LOCK_ID);
        begin_lock_scope(9);
        finish_lock_scope(9);
        finish_lock_scope(UNMANAGED_LOCK_ID);
        assert_eq!(class_of(9), 9);

        begin_lock_scope(10);
        begin_lock_scope(UNMANAGED_LOCK_ID);
        finish_lock_scope(UNMANAGED_LOCK_ID);
        finish_lock_scope(10);
        assert_eq!(class_of(10), 10);
    }

    #[test]
    fn admission_word_publishes_the_class_of_a_merged_lock() {
        let _state = isolate_dependency_state();
        reset_state();

        union_lock_classes(9, 4);
        assert_eq!(class_of(9), 4);

        let scope = begin_lock_scope(9);
        assert!(mark_slow_path_pending_for_scope(scope));
        assert_eq!(word_for_test(), word_for(4, SLOW_PATH_PENDING));

        mark_critical_section_entered_for_scope(scope);
        assert_eq!(word_for_test(), word_for(4, IN_CRITICAL_SECTION));

        finish_lock_scope(9);
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        // The consumed token belongs to the class, so the sibling lock of the
        // same component reuses it instead of dropping it.
        let sibling = begin_lock_scope(4);
        assert!(token_consumed_for_scope(sibling));
        finish_lock_scope(4);

        reset_state();
    }

    #[test]
    fn nested_reacquire_of_the_same_lock_creates_no_edge() {
        let _state = isolate_dependency_state();

        begin_lock_scope(4);
        begin_lock_scope(4);
        finish_lock_scope(4);
        finish_lock_scope(4);

        assert_eq!(class_of(4), 4);
        assert!(!take_classes_dirty());
    }

    #[test]
    fn transition_counts_follow_outermost_class_handoffs() {
        let _state = isolate_dependency_state();
        force_dump_for_test(true);

        begin_lock_scope(2);
        finish_lock_scope(2);
        begin_lock_scope(3);
        finish_lock_scope(3);
        begin_lock_scope(2);
        finish_lock_scope(2);

        assert_eq!(transition_count_for_test(2, 3), 1);
        assert_eq!(transition_count_for_test(3, 2), 1);
        assert_eq!(transition_count_for_test(2, 2), 0);

        begin_lock_scope(5);
        begin_lock_scope(6);
        finish_lock_scope(6);
        finish_lock_scope(5);

        assert_eq!(transition_count_for_test(2, 5), 1);
        assert_eq!(transition_count_for_test(5, 6), 0);

        // Lock 6 was merged into class 5 by the nesting above, so its next
        // outermost acquire is counted against the class, not the lock.
        begin_lock_scope(6);
        finish_lock_scope(6);

        assert_eq!(class_of(6), 5);
        assert_eq!(transition_count_for_test(5, 5), 1);
        assert_eq!(transition_count_for_test(5, 6), 0);
    }
}
