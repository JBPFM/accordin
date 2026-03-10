//! Per-thread epoch aggregate state and mmapable slot management.
//!
//! Each thread owns one slot in the `epoch_slots` BPF array (pinned at
//! `/sys/fs/bpf/ulock_epoch_slots`).  The thread writes exclusively to its
//! own slot using a seqlock-style version field; the controller reads all
//! slots periodically.
//!
//! If the BPF map is not available (scheduler not running), writes are
//! silently dropped so the lock library remains usable standalone.

use std::cell::UnsafeCell;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering, fence};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of slots in the epoch_slots map.  Must match intf.h.
pub const MAX_SLOTS: u32 = 1024;

/// Default epoch duration in nanoseconds (20 ms).  Overridden by
/// ULOCK_EPOCH_MS environment variable if set.
const DEFAULT_EPOCH_NS: u64 = 20_000_000;

/// Path where the controller pins the epoch_slots BPF map.
const EPOCH_SLOTS_PIN_PATH: &[u8] = b"/sys/fs/bpf/ulock_epoch_slots\0";

// ---------------------------------------------------------------------------
// Slot layout (must match `struct epoch_slot` in intf.h exactly)
// ---------------------------------------------------------------------------

/// Mirrors `struct epoch_slot` in `src/bpf/intf.h`.
/// SAFETY: This struct is shared between the lock library (writer) and the
/// controller (reader) through a memory-mapped BPF array.  Field order and
/// sizes must match the C definition precisely.
#[repr(C)]
pub struct EpochSlot {
    pub tid:            u32,
    pub tgid:           u32,
    pub slot_id:        u32,
    pub epoch_id:       u32,
    pub lock_domain_id: u32,
    pub _pad:           u32,
    pub wait_ns:        u64,
    pub hold_ns:        u64,
    pub park_ns:        u64,
    pub contended_acq:  u64,
    pub park_count:     u64,
    /// Seqlock version: odd = write in progress, even = snapshot committed.
    pub seq:            AtomicU64,
    pub flags:          u64,
}

// ---------------------------------------------------------------------------
// Thread-local epoch accumulator
// ---------------------------------------------------------------------------

/// Per-thread epoch state.  Lives entirely in thread-local storage so no
/// synchronisation is needed for the accumulator fields.
pub struct EpochState {
    // Per-epoch accumulators.  Reset after each flush.
    wait_ns:       u64,
    hold_ns:       u64,
    park_ns:       u64,
    contended_acq: u64,
    park_count:    u64,

    // Timing helpers (not written to slot directly).
    spin_start_ns: u64, // monotonic ns when slow-path spin began
    lock_start_ns: u64, // monotonic ns when lock was acquired (for hold_ns)
    park_start_ns: u64, // monotonic ns when futex_wait entered

    // Epoch bookkeeping.
    epoch_id:       u32,
    epoch_start_ns: u64,
    epoch_ns:       u64,

    // Slot pointer (null if BPF map unavailable).
    slot:    *mut EpochSlot,
    slot_id: u32,
    tid:     u32,
    tgid:    u32,
}

// SAFETY: EpochState is always accessed from a single thread via thread_local!.
unsafe impl Send for EpochState {}
unsafe impl Sync for EpochState {}

impl EpochState {
    /// Initialise a new epoch state for the calling thread.
    pub fn new() -> Self {
        let tid  = gettid();
        let tgid = unsafe { libc::getpid() as u32 };

        // Assign slot by tid hash.  Slot 0 is reserved; use slots 1 .. MAX_SLOTS-1.
        let slot_id = (tid % (MAX_SLOTS - 1)) + 1;

        // Determine epoch duration from env or default.
        let epoch_ns = epoch_ns_from_env();

        // Try to open the BPF map and get a pointer to our slot.
        let slot = open_slot(slot_id as usize).unwrap_or(ptr::null_mut());

        // Initialise the slot header if we have one.
        if !slot.is_null() {
            let s = unsafe { &mut *slot };
            s.tid     = tid;
            s.tgid    = tgid;
            s.slot_id = slot_id;
            s.seq.store(0, Ordering::Relaxed);
        }

        Self {
            wait_ns:       0,
            hold_ns:       0,
            park_ns:       0,
            contended_acq: 0,
            park_count:    0,
            spin_start_ns: 0,
            lock_start_ns: 0,
            park_start_ns: 0,
            epoch_id:       0,
            epoch_start_ns: now_ns(),
            epoch_ns,
            slot,
            slot_id,
            tid,
            tgid,
        }
    }

    // -----------------------------------------------------------------------
    // Instrumentation call sites (called from the hooked lock functions)
    // -----------------------------------------------------------------------

    /// Called when the lock slow path is entered (CAS acquisition failed).
    /// Records the start of the wait interval and bumps contended_acq.
    #[inline(always)]
    pub fn on_contention_start(&mut self) {
        self.spin_start_ns = now_ns();
        self.contended_acq += 1;
    }

    /// Called immediately after the lock is acquired.
    /// `was_contended` is true if the slow path was taken.
    #[inline(always)]
    pub fn on_lock_acquired(&mut self, was_contended: bool) {
        let now = now_ns();
        if was_contended && self.spin_start_ns != 0 {
            self.wait_ns += now.saturating_sub(self.spin_start_ns);
            self.spin_start_ns = 0;
        }
        self.lock_start_ns = now;
        self.maybe_flush_epoch(now);
    }

    /// Called when the lock is released.
    #[inline(always)]
    pub fn on_lock_released(&mut self) {
        if self.lock_start_ns != 0 {
            self.hold_ns += now_ns().saturating_sub(self.lock_start_ns);
            self.lock_start_ns = 0;
        }
    }

    /// Called immediately before futex_wait (parking).
    #[inline(always)]
    pub fn on_park_start(&mut self) {
        self.park_start_ns = now_ns();
    }

    /// Called immediately after futex_wait returns (woken).
    #[inline(always)]
    pub fn on_park_end(&mut self) {
        if self.park_start_ns != 0 {
            self.park_ns    += now_ns().saturating_sub(self.park_start_ns);
            self.park_count += 1;
            self.park_start_ns = 0;
        }
    }

    // -----------------------------------------------------------------------
    // Epoch management
    // -----------------------------------------------------------------------

    /// Flush epoch accumulators to the shared slot if the epoch has elapsed.
    #[inline(always)]
    fn maybe_flush_epoch(&mut self, now: u64) {
        if now.saturating_sub(self.epoch_start_ns) < self.epoch_ns {
            return;
        }
        self.flush_and_reset(now);
    }

    /// Write current accumulators to the slot under seqlock protocol, then
    /// reset accumulators and advance to the next epoch.
    fn flush_and_reset(&mut self, now: u64) {
        if !self.slot.is_null() {
            self.write_slot_seqlock();
        }
        // Reset accumulators.
        self.wait_ns       = 0;
        self.hold_ns       = 0;
        self.park_ns       = 0;
        self.contended_acq = 0;
        self.park_count    = 0;
        self.epoch_id      = self.epoch_id.wrapping_add(1);
        self.epoch_start_ns = now;
    }

    /// Write snapshot to the slot using seqlock protocol.
    ///
    /// Protocol:
    ///   1. Increment seq to an odd value (write in progress).
    ///   2. Write all payload fields.
    ///   3. Increment seq to the next even value (snapshot committed).
    ///
    /// The controller reads seq before and after reading the payload; it
    /// retries if seq changed or is odd.
    fn write_slot_seqlock(&self) {
        let slot = unsafe { &*self.slot };

        // Step 1: begin write (make seq odd).
        let old_seq = slot.seq.load(Ordering::Relaxed);
        slot.seq.store(old_seq | 1, Ordering::Relaxed);
        fence(Ordering::Release);

        // Step 2: write payload.
        // SAFETY: Only this thread writes to this slot.
        let slot_mut = unsafe { &mut *(self.slot) };
        slot_mut.epoch_id       = self.epoch_id;
        slot_mut.wait_ns        = self.wait_ns;
        slot_mut.hold_ns        = self.hold_ns;
        slot_mut.park_ns        = self.park_ns;
        slot_mut.contended_acq  = self.contended_acq;
        slot_mut.park_count     = self.park_count;
        slot_mut.lock_domain_id = 0; // extended in Phase 3

        // Step 3: commit (make seq even).
        fence(Ordering::Release);
        slot.seq.store(old_seq.wrapping_add(2), Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Thread-local accessor
// ---------------------------------------------------------------------------

thread_local! {
    static EPOCH: UnsafeCell<EpochState> = UnsafeCell::new(EpochState::new());
}

/// Execute a closure with a mutable reference to the calling thread's epoch state.
///
/// This avoids the overhead of RefCell by using UnsafeCell with a static
/// thread-local — the single-thread access invariant is upheld by construction.
#[inline(always)]
pub fn with_epoch<F>(f: F)
where
    F: FnOnce(&mut EpochState),
{
    EPOCH.with(|cell| {
        // SAFETY: thread_local guarantees single-thread access.
        let state = unsafe { &mut *cell.get() };
        f(state);
    });
}

// ---------------------------------------------------------------------------
// Low-level helpers
// ---------------------------------------------------------------------------

/// Read the monotonic clock in nanoseconds.
#[inline(always)]
fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) };
    (ts.tv_sec as u64).wrapping_mul(1_000_000_000).wrapping_add(ts.tv_nsec as u64)
}

/// Get the calling thread's kernel TID via gettid(2).
fn gettid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

/// Read the epoch duration from ULOCK_EPOCH_MS or use the default (20 ms).
fn epoch_ns_from_env() -> u64 {
    std::env::var("ULOCK_EPOCH_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| ms * 1_000_000)
        .unwrap_or(DEFAULT_EPOCH_NS)
}

/// Open the epoch_slots BPF map FD and return a pointer to slot `slot_id`.
///
/// Two strategies are tried in order:
///   1. `ULOCK_EPOCH_SLOTS_FD` environment variable set by the controller
///      before forking the child.  The FD is already open and mmap-ready.
///   2. `bpf_obj_get` on the pinned path for independently-started workloads.
///
/// Returns None if neither strategy succeeds (scheduler not loaded).
fn open_slot(slot_id: usize) -> Option<*mut EpochSlot> {
    let fd = slot_fd_from_env().or_else(|| bpf_obj_get(EPOCH_SLOTS_PIN_PATH))?;

    let slot_size  = mem::size_of::<EpochSlot>();
    let total_size = (MAX_SLOTS as usize) * slot_size;

    // Round up to page boundary (mmap requirement).
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let mmap_size = (total_size + page - 1) & !(page - 1);

    let base = unsafe {
        libc::mmap(
            ptr::null_mut(),
            mmap_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };

    unsafe { libc::close(fd) };

    if base == libc::MAP_FAILED {
        return None;
    }

    let slots = base as *mut EpochSlot;
    Some(unsafe { slots.add(slot_id) })
}

/// Read a map FD from the `ULOCK_EPOCH_SLOTS_FD` environment variable.
/// The controller sets this before forking the child workload so the FD
/// is inherited across fork+exec without needing a BPF pin.
fn slot_fd_from_env() -> Option<i32> {
    let val = std::env::var("ULOCK_EPOCH_SLOTS_FD").ok()?;
    let fd: i32 = val.trim().parse().ok()?;
    if fd < 0 { None } else { Some(fd) }
}

/// Issue `bpf(BPF_OBJ_GET, path)` and return the file descriptor on success.
///
/// Uses the raw syscall so the lock library does not need to link libbpf.
fn bpf_obj_get(path: &[u8]) -> Option<i32> {
    // BPF_OBJ_GET = 7
    // struct { __aligned_u64 pathname; __u32 bpf_fd; __u32 file_flags; ... }
    // We only need the first 16 bytes.
    #[repr(C, align(8))]
    struct Attr {
        pathname:   u64,
        bpf_fd:     u32,
        file_flags: u32,
    }

    let attr = Attr {
        pathname:   path.as_ptr() as u64,
        bpf_fd:     0,
        file_flags: 0,
    };

    let fd = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            7i32,                                   // BPF_OBJ_GET
            &attr as *const Attr as *const libc::c_void,
            mem::size_of::<Attr>() as u32,
        )
    };

    if fd < 0 { None } else { Some(fd as i32) }
}
