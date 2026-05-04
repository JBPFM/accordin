use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::admission::{
    mark_critical_section_entered, mark_critical_section_exit, mark_slow_path_pending,
};
use crate::arch::{CacheAligned, pause};
use crate::lock_backend::LockBackend;

#[repr(C, align(128))]
pub struct WaitElement {
    gate: AtomicPtr<WaitElement>,
}

impl WaitElement {
    pub const fn new() -> Self {
        Self {
            gate: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

thread_local! {
    static THREAD_ELEMENT: WaitElement = const { WaitElement::new() };
    static HELD_LOCK: Cell<Option<HeldLockContext>> = const { Cell::new(None) };
    static HELD_LOCKS: RefCell<Vec<HeldLockContext>> = const { RefCell::new(Vec::new()) };
}

const LOCKED_EMPTY: *mut WaitElement = 1usize as *mut WaitElement;

/// Reciprocating lock with cache-aligned shared state variables.
pub struct ReciprocatingLockRaw {
    arv: CacheAligned<AtomicPtr<WaitElement>>,
}

#[derive(Clone, Copy)]
struct LockContext {
    succ: *mut WaitElement,
    eos: *mut WaitElement,
}

#[derive(Clone, Copy)]
struct HeldLockContext {
    lock: *const ReciprocatingLockRaw,
    context: LockContext,
}

fn push_lock_context(lock: *const ReciprocatingLockRaw, context: LockContext) {
    let held_context = HeldLockContext { lock, context };
    HELD_LOCK.with(|held| {
        if held.get().is_none() {
            held.set(Some(held_context));
        } else {
            HELD_LOCKS.with(|held| {
                held.borrow_mut().push(held_context);
            });
        }
    });
}

fn pop_lock_context(lock: *const ReciprocatingLockRaw) -> LockContext {
    if let Some(context) = HELD_LOCK.with(|held| match held.get() {
        Some(context) if ptr::eq(context.lock, lock) => {
            held.set(None);
            Some(context.context)
        }
        _ => None,
    }) {
        return context;
    }

    HELD_LOCKS.with(|held| {
        let mut held = held.borrow_mut();
        let pos = held
            .iter()
            .rposition(|context| ptr::eq(context.lock, lock))
            .expect("unlock called without a matching reciprocating lock acquisition");
        held.swap_remove(pos).context
    })
}

impl ReciprocatingLockRaw {
    pub const fn new() -> Self {
        Self {
            arv: CacheAligned(AtomicPtr::new(ptr::null_mut())),
        }
    }

    #[inline(always)]
    fn thread_element() -> *mut WaitElement {
        THREAD_ELEMENT.with(|element| {
            let ptr = element as *const WaitElement as *mut WaitElement;
            debug_assert_eq!((ptr as usize) & 1, 0);
            ptr
        })
    }

    #[inline(always)]
    fn untag_low_bit(ptr: *mut WaitElement) -> *mut WaitElement {
        ((ptr as usize) & !1usize) as *mut WaitElement
    }

    #[inline(always)]
    fn ensure_slow_path_admission(&self) {
        mark_slow_path_pending();
        std::thread::yield_now();
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn acquire_context(&self) -> LockContext {
        let element = Self::thread_element();
        unsafe {
            (*element).gate.store(ptr::null_mut(), Ordering::Relaxed);
        }

        let tail = self.arv.0.swap(element, Ordering::AcqRel);
        debug_assert_ne!(tail, element);

        let mut succ = ptr::null_mut();
        let mut eos = element;

        if !tail.is_null() {
            succ = Self::untag_low_bit(tail);
            debug_assert_ne!(succ, element);

            loop {
                eos = unsafe { (*element).gate.load(Ordering::Acquire) };
                if !eos.is_null() {
                    break;
                }
                pause();
            }

            debug_assert_ne!(eos, element);
            if succ == eos {
                succ = ptr::null_mut();
                eos = LOCKED_EMPTY;
            }
        }

        LockContext { succ, eos }
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn lock_slow(&self) {
        self.ensure_slow_path_admission();
        let context = self.acquire_context();
        push_lock_context(self, context);
        mark_critical_section_entered();
    }

    /// Returns true if the lock was acquired, false if it was already held.
    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn try_lock_fast(&self) -> bool {
        let element = Self::thread_element();
        unsafe {
            (*element).gate.store(ptr::null_mut(), Ordering::Relaxed);
        }

        if self
            .arv
            .0
            .compare_exchange(
                ptr::null_mut(),
                element,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return false;
        }

        push_lock_context(
            self,
            LockContext {
                succ: ptr::null_mut(),
                eos: element,
            },
        );
        true
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    unsafe fn unlock_with_context(&self, context: LockContext) {
        debug_assert!(!context.eos.is_null());

        if !context.succ.is_null() {
            debug_assert_ne!(context.succ, LOCKED_EMPTY);
            unsafe {
                (*context.succ).gate.store(context.eos, Ordering::Release);
            }
            return;
        }

        if self
            .arv
            .0
            .compare_exchange(
                context.eos,
                ptr::null_mut(),
                Ordering::Release,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            return;
        }

        let waiter = self.arv.0.swap(LOCKED_EMPTY, Ordering::AcqRel);

        debug_assert!(!waiter.is_null());
        debug_assert_ne!(waiter, LOCKED_EMPTY);
        debug_assert_ne!(waiter, context.eos);
        unsafe {
            (*waiter).gate.store(context.eos, Ordering::Release);
        }
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn unlock_fast(&self) {
        let context = pop_lock_context(self);
        unsafe {
            self.unlock_with_context(context);
        }
        mark_critical_section_exit();
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn lock_guard(&self) -> ReciprocatingGuard<'_> {
        ReciprocatingGuard {
            lock: self,
            context: Some(self.acquire_context()),
            _not_send: PhantomData,
        }
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub fn is_locked_relaxed(&self) -> bool {
        !self.arv.0.load(Ordering::Relaxed).is_null()
    }
}

impl LockBackend for ReciprocatingLockRaw {
    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn lock(&self) {
        self.lock_slow();
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn try_lock(&self) -> bool {
        self.try_lock_fast()
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn unlock(&self) {
        self.unlock_fast();
    }
}

impl Default for ReciprocatingLockRaw {
    fn default() -> Self {
        Self::new()
    }
}

#[must_use = "dropping the guard immediately unlocks the ReciprocatingLockRaw"]
#[allow(dead_code)]
pub struct ReciprocatingGuard<'a> {
    lock: &'a ReciprocatingLockRaw,
    context: Option<LockContext>,
    _not_send: PhantomData<*mut ()>,
}

impl ReciprocatingGuard<'_> {
    #[inline(always)]
    #[allow(dead_code)]
    pub fn unlock(mut self) {
        if let Some(context) = self.context.take() {
            unsafe {
                self.lock.unlock_with_context(context);
            }
        }
    }
}

impl Drop for ReciprocatingGuard<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        if let Some(context) = self.context.take() {
            unsafe {
                self.lock.unlock_with_context(context);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::UnsafeCell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::{ReciprocatingLockRaw, WaitElement};
    use crate::lock_backend::LockBackend;

    #[test]
    fn wait_element_layout_stays_cache_aligned() {
        assert_eq!(std::mem::size_of::<WaitElement>(), 128);
        assert_eq!(std::mem::align_of::<WaitElement>(), 128);
    }

    #[test]
    fn try_lock_requires_unlock_before_reacquire() {
        let lock = ReciprocatingLockRaw::new();

        assert!(lock.try_lock());
        assert!(!lock.try_lock());
        lock.unlock();
        assert!(lock.try_lock());
        lock.unlock();
    }

    struct SharedCounter {
        lock: ReciprocatingLockRaw,
        value: UnsafeCell<usize>,
    }

    unsafe impl Sync for SharedCounter {}

    #[test]
    fn lock_serializes_counter_updates() {
        let shared = Arc::new(SharedCounter {
            lock: ReciprocatingLockRaw::new(),
            value: UnsafeCell::new(0),
        });
        let mut handles = Vec::new();

        for _ in 0..4 {
            let shared = Arc::clone(&shared);
            handles.push(std::thread::spawn(move || {
                for _ in 0..2_000 {
                    if !shared.lock.try_lock() {
                        shared.lock.lock();
                    }
                    unsafe {
                        let next = (*shared.value.get()).wrapping_add(1);
                        *shared.value.get() = next;
                    }
                    shared.lock.unlock();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(unsafe { *shared.value.get() }, 8_000);
    }

    #[test]
    fn lock_does_not_deadlock_under_contention() {
        const DEFAULT_THREADS: usize = 32;
        const DEFAULT_ITERATIONS: usize = 2_000;

        let threads = std::env::var("RECIPROCATING_TEST_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_THREADS)
            .max(2);

        let iterations = std::env::var("RECIPROCATING_TEST_ITERATIONS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_ITERATIONS)
            .max(100);

        let shared = Arc::new(SharedCounter {
            lock: ReciprocatingLockRaw::new(),
            value: UnsafeCell::new(0),
        });
        let done = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);

        for _ in 0..threads {
            let shared = Arc::clone(&shared);
            let done = Arc::clone(&done);
            let started = Arc::clone(&started);
            let start = Arc::clone(&start);
            let iterations = iterations;

            handles.push(std::thread::spawn(move || {
                while started.load(Ordering::Acquire) == 0 {
                    std::thread::yield_now();
                }

                for _ in 0..iterations {
                    if !shared.lock.try_lock() {
                        shared.lock.lock();
                    }
                    unsafe {
                        let mut_guard = &mut *shared.value.get();
                        *mut_guard = mut_guard.wrapping_add(1);
                    }
                    shared.lock.unlock();
                    if done.load(Ordering::Acquire) {
                        return;
                    }
                }

                start.fetch_add(1, Ordering::AcqRel);
            }));
        }

        started.store(1, Ordering::Release);
        while start.load(Ordering::Acquire) < threads {
            assert!(
                Instant::now() < deadline,
                "reciprocating lock did not complete under contention"
            );
            std::thread::yield_now();
        }

        done.store(true, Ordering::Release);
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(unsafe { *shared.value.get() }, threads * iterations);
    }
}
