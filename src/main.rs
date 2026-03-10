// SPDX-License-Identifier: GPL-2.0-only
//
// scx_ulock user-space controller
//
// Responsibilities:
//   - Load and attach the BPF scheduler.
//   - Initialise shared BPF maps (config, SSC mask, epoch slots).
//   - Run the control loop: collect epoch slots, classify tasks,
//     update SSC width, push results back into BPF maps.
//   - Optionally launch a child workload and exit when it finishes.

mod bpf_skel;
pub use bpf_skel::*;
pub mod bpf_intf;
mod classify;
mod search;
mod topo;

use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::io::{AsFd, AsRawFd};
use std::sync::atomic::Ordering as AtomicOrdering;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use libbpf_rs::{Link, MapCore, MapFlags, OpenObject};
use libc::pid_t;
use log::{info, debug};
use scx_utils::{scx_ops_attach, scx_ops_load, scx_ops_open};

const SCHEDULER_NAME: &str = "scx_ulock";

// Default tuning constants (all overridable via CLI).
const DEFAULT_EPOCH_MS: u64    = 20;
const DEFAULT_CONTROL_MS: u64  = 100;
const DEFAULT_ENTER_PCT: u32   = 10;
const DEFAULT_EXIT_PCT: u32    = 5;
const DEFAULT_MIN_CONTENDED: u32 = 64;
const DEFAULT_HOT_EPOCHS: u32  = 3;
const DEFAULT_COOL_EPOCHS: u32 = 5;
const DEFAULT_MAX_SSC: u32     = 32;

/// scx_ulock: user-space lock contention-aware scheduler
#[derive(Debug, Parser, Clone)]
#[command(trailing_var_arg = true)]
struct Opts {
    /// Enable partial mode: only manage tasks that opt-in via SCHED_EXT
    #[clap(long, action = clap::ArgAction::SetTrue)]
    partial: bool,

    /// Epoch duration in milliseconds
    #[clap(long, default_value_t = DEFAULT_EPOCH_MS)]
    epoch_ms: u64,

    /// Controller wakeup period in milliseconds
    #[clap(long, default_value_t = DEFAULT_CONTROL_MS)]
    control_ms: u64,

    /// wait_ratio% threshold to enter SSC (0-100)
    #[clap(long, default_value_t = DEFAULT_ENTER_PCT)]
    enter_pct: u32,

    /// wait_ratio% threshold to leave SSC (0-100)
    #[clap(long, default_value_t = DEFAULT_EXIT_PCT)]
    exit_pct: u32,

    /// Minimum contended acquisitions per epoch to enter SSC
    #[clap(long, default_value_t = DEFAULT_MIN_CONTENDED)]
    min_contended: u32,

    /// Consecutive hot epochs required to enter SSC
    #[clap(long, default_value_t = DEFAULT_HOT_EPOCHS)]
    hot_epochs: u32,

    /// Consecutive cool epochs required to leave SSC
    #[clap(long, default_value_t = DEFAULT_COOL_EPOCHS)]
    cool_epochs: u32,

    /// Maximum allowed SSC width (in CPUs)
    #[clap(long, default_value_t = DEFAULT_MAX_SSC)]
    max_ssc: u32,

    /// Restrict scheduling to this cgroup path (optional)
    #[clap(long)]
    target_cgroup: Option<String>,

    /// Comma-separated CPU list to use for scheduling (e.g. "0-7,12")
    #[clap(long)]
    cpu_list: Option<String>,

    /// Write per-epoch metrics to this file (default: stderr)
    #[clap(long)]
    metrics_out: Option<String>,

    /// Enable rseq-based timeslice extension (reserved for Phase 3)
    #[clap(long, action = clap::ArgAction::SetTrue)]
    enable_rseq_slice_ext: bool,

    /// Enable verbose output
    #[clap(short = 'v', long, action = clap::ArgAction::SetTrue)]
    verbose: bool,

    /// Enable debug output
    #[clap(short = 'd', long, action = clap::ArgAction::SetTrue)]
    debug: bool,

    /// Optional command to launch under the scheduler (use -- to separate)
    #[clap(value_name = "CMD", last = true)]
    command: Vec<String>,
}

// ---------------------------------------------------------------------------
// BPF scheduler wrapper
// ---------------------------------------------------------------------------

struct Scheduler {
    _link: Link,
    /// Dup'd epoch_slots FD with CLOEXEC cleared for child inheritance.
    epoch_slots_fd: i32,
}

impl Scheduler {
    fn init(opts: &Opts, open_object: &mut MaybeUninit<OpenObject>) -> Result<Self> {
        let mut skel_builder = BpfSkelBuilder::default();
        skel_builder.obj_builder.debug(opts.debug);

        let mut skel = scx_ops_open!(skel_builder, open_object, ulock_ops, None)?;
        let mut skel = scx_ops_load!(skel, ulock_ops, uei)?;

        // Write initial global config before attaching.
        Self::write_initial_config(&mut skel, opts)?;

        // Dup epoch_slots FD and clear CLOEXEC so the child inherits it.
        let epoch_slots_fd = {
            let raw = skel.maps.epoch_slots.as_fd().as_raw_fd();
            let dup = unsafe { libc::dup(raw) };
            if dup >= 0 {
                unsafe { libc::fcntl(dup, libc::F_SETFD, 0) };
            }
            dup
        };

        // Best-effort: pin the map so independently-started workloads can find it.
        let _ = std::fs::create_dir_all("/sys/fs/bpf");
        if let Err(e) = skel.maps.epoch_slots.pin("/sys/fs/bpf/ulock_epoch_slots") {
            debug!("epoch_slots pin failed (not fatal): {e}");
        }

        let _link = scx_ops_attach!(skel, ulock_ops)?;

        info!("{SCHEDULER_NAME} attached (partial={}, epoch={}ms, control={}ms)",
              opts.partial, opts.epoch_ms, opts.control_ms);
        Ok(Self { _link, epoch_slots_fd })
    }

    /// Push the initial ulock_config into BPF map index 0.
    fn write_initial_config(skel: &mut BpfSkel, opts: &Opts) -> Result<()> {
        let cfg = bpf_intf::ulock_config {
            epoch_ns:            opts.epoch_ms   * 1_000_000,
            control_period_ns:   opts.control_ms * 1_000_000,
            enter_threshold_pct: opts.enter_pct,
            exit_threshold_pct:  opts.exit_pct,
            min_contended_acq:   opts.min_contended,
            hot_epochs_needed:   opts.hot_epochs,
            cool_epochs_needed:  opts.cool_epochs,
            ssc_width:           0,
            max_ssc_width:       opts.max_ssc,
            partial_mode:        opts.partial as u32,
            ssc_gen:             0,
        };

        let key_bytes   = 0u32.to_ne_bytes();
        let value_bytes = unsafe {
            std::slice::from_raw_parts(
                &cfg as *const _ as *const u8,
                std::mem::size_of::<bpf_intf::ulock_config>(),
            )
        };

        skel.maps
            .ulock_config_map
            .update(&key_bytes, value_bytes, MapFlags::ANY)
            .context("failed to write initial ulock_config")?;

        debug!("initial config written to BPF map");
        Ok(())
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        let _ = std::fs::remove_file("/sys/fs/bpf/ulock_epoch_slots");
        if self.epoch_slots_fd >= 0 {
            unsafe { libc::close(self.epoch_slots_fd) };
        }
        info!("{SCHEDULER_NAME} detached");
    }
}

// ---------------------------------------------------------------------------
// Epoch slot reader (controller side)
// ---------------------------------------------------------------------------

/// Consistent snapshot of one epoch slot read under seqlock protocol.
#[derive(Debug, Clone, Default)]
struct EpochSnapshot {
    pub tid:           u32,
    pub tgid:          u32,
    pub slot_id:       u32,
    pub epoch_id:      u32,
    pub wait_ns:       u64,
    pub hold_ns:       u64,
    pub park_ns:       u64,
    pub contended_acq: u64,
    pub park_count:    u64,
}

/// Reads epoch slots from the mmapped BPF array.
struct EpochReader {
    base:       *const u8,
    slot_count: usize,
    slot_size:  usize,
    mmap_size:  usize,
}

unsafe impl Send for EpochReader {}

impl EpochReader {
    fn new(fd: i32) -> Option<Self> {
        if fd < 0 { return None; }

        let slot_count = bpf_intf::MAX_SLOTS as usize;
        let slot_size  = std::mem::size_of::<bpf_intf::epoch_slot>();
        let page       = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let mmap_size  = (slot_count * slot_size + page - 1) & !(page - 1);

        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mmap_size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };

        if base == libc::MAP_FAILED { return None; }

        Some(Self { base: base as *const u8, slot_count, slot_size, mmap_size })
    }

    /// Read all non-empty slots under seqlock protocol.
    fn collect(&self) -> Vec<EpochSnapshot> {
        let mut out = Vec::new();
        for i in 0..self.slot_count {
            let slot = unsafe {
                &*(self.base.add(i * self.slot_size) as *const bpf_intf::epoch_slot)
            };

            if slot.tid == 0 { continue; }

            // Seqlock retry loop.  The BPF-generated seq field is u64; treat
            // it as AtomicU64 for acquire/release ordering.
            let seq_atom = unsafe {
                &*((&slot.seq as *const u64) as *const std::sync::atomic::AtomicU64)
            };
            let snap = loop {
                let seq1 = seq_atom.load(AtomicOrdering::Acquire);
                if seq1 & 1 != 0 { std::hint::spin_loop(); continue; }

                let s = EpochSnapshot {
                    tid:           slot.tid,
                    tgid:          slot.tgid,
                    slot_id:       slot.slot_id,
                    epoch_id:      slot.epoch_id,
                    wait_ns:       slot.wait_ns,
                    hold_ns:       slot.hold_ns,
                    park_ns:       slot.park_ns,
                    contended_acq: slot.contended_acq,
                    park_count:    slot.park_count,
                };

                let seq2 = seq_atom.load(AtomicOrdering::Acquire);
                if seq2 == seq1 { break s; }
            };
            out.push(snap);
        }
        out
    }
}

impl Drop for EpochReader {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.mmap_size) };
    }
}

// ---------------------------------------------------------------------------
// Child process management
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct ChildProcess {
    pid: pid_t,
}

impl ChildProcess {
    /// Fork a child, immediately stop it with SIGSTOP, and return.
    fn spawn_suspended(command: &[String]) -> Result<Self> {
        if command.is_empty() {
            bail!("launch command is empty");
        }

        let cstrings: Vec<CString> = command
            .iter()
            .map(|arg| {
                CString::new(arg.as_str())
                    .with_context(|| format!("argument '{}' contains NUL byte", arg))
            })
            .collect::<Result<_, _>>()?;

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error()).context("fork failed");
        }

        if pid == 0 {
            // Child: stop self, then exec.
            unsafe {
                if libc::raise(libc::SIGSTOP) != 0 {
                    libc::_exit(127);
                }
                let mut argv: Vec<*const libc::c_char> =
                    cstrings.iter().map(|s| s.as_ptr()).collect();
                argv.push(ptr::null());
                libc::execvp(cstrings[0].as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }

        // Parent: wait until child stops.
        wait_for_child_stop(pid)?;
        Ok(Self { pid })
    }

    fn resume(&self) -> Result<()> {
        if unsafe { libc::kill(self.pid, libc::SIGCONT) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("SIGCONT to {} failed", self.pid));
        }
        Ok(())
    }

    /// Non-blocking check: returns true if the child has exited.
    fn has_exited(&self) -> bool {
        let mut status: libc::c_int = 0;
        let ret = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if ret < 0 {
            return true; // treat error as "gone"
        }
        if ret == 0 {
            return false; // still running
        }
        if libc::WIFEXITED(status) {
            info!("child {} exited with status {}", self.pid, libc::WEXITSTATUS(status));
        } else if libc::WIFSIGNALED(status) {
            info!("child {} killed by signal {}", self.pid, libc::WTERMSIG(status));
        }
        true
    }
}

fn wait_for_child_stop(pid: pid_t) -> Result<()> {
    let mut status: libc::c_int = 0;
    loop {
        let ret = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err)
                .with_context(|| format!("waitpid for {} failed", pid));
        }
        if libc::WIFSTOPPED(status) {
            return Ok(());
        }
        if libc::WIFEXITED(status) {
            bail!("child {} exited (status {}) before SIGCONT",
                  pid, libc::WEXITSTATUS(status));
        }
        if libc::WIFSIGNALED(status) {
            bail!("child {} killed by signal {} before SIGCONT",
                  pid, libc::WTERMSIG(status));
        }
    }
}

// ---------------------------------------------------------------------------
// Controller loop
// ---------------------------------------------------------------------------

struct Controller {
    control_period: Duration,
    epoch_reader:   Option<EpochReader>,
    verbose:        bool,
    // Phase 2: Classifier, SscSearch, CpuTopo will be added here.
}

impl Controller {
    fn new(opts: &Opts, epoch_slots_fd: i32) -> Self {
        let epoch_reader = EpochReader::new(epoch_slots_fd);
        if epoch_reader.is_none() {
            debug!("epoch_reader unavailable (epoch_slots mmap failed)");
        }
        Self {
            control_period: Duration::from_millis(opts.control_ms),
            epoch_reader,
            verbose: opts.verbose,
        }
    }

    /// Run one control iteration.
    ///
    /// Phase 1: collect epoch slots and log aggregate metrics.
    /// Phase 2: classify tasks, update task_class_map, adjust SSC width.
    fn tick(&mut self) {
        let Some(ref reader) = self.epoch_reader else {
            debug!("controller tick: no epoch reader");
            return;
        };

        let snapshots = reader.collect();
        if snapshots.is_empty() { return; }

        // Aggregate across all active slots.
        let total_wait_ns:  u64 = snapshots.iter().map(|s| s.wait_ns).sum();
        let total_hold_ns:  u64 = snapshots.iter().map(|s| s.hold_ns).sum();
        let total_park_ns:  u64 = snapshots.iter().map(|s| s.park_ns).sum();
        let total_contended: u64 = snapshots.iter().map(|s| s.contended_acq).sum();
        let n = snapshots.len() as u64;

        if self.verbose && n > 0 {
            info!("epoch: threads={n} wait_ns={total_wait_ns} \
                   hold_ns={total_hold_ns} park_ns={total_park_ns} \
                   contended_acq={total_contended}");
        }

        // TODO(phase2): per-task classification state machine
        // TODO(phase2): update task_class_map in BPF
        // TODO(phase2): ssc_search.step() → update ssc_cpumask + bump ssc_gen
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let opts = Opts::parse();

    // Initialise logger.
    let log_level = if opts.debug {
        simplelog::LevelFilter::Debug
    } else if opts.verbose {
        simplelog::LevelFilter::Info
    } else {
        simplelog::LevelFilter::Warn
    };
    simplelog::TermLogger::init(
        log_level,
        simplelog::Config::default(),
        simplelog::TerminalMode::Stderr,
        simplelog::ColorChoice::Auto,
    )
    .context("failed to initialise logger")?;

    // Ctrl-C / SIGTERM shutdown flag.
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    ctrlc::set_handler(move || {
        shutdown_clone.store(true, Ordering::Relaxed);
    })
    .context("failed to set Ctrl-C handler")?;

    // Load and attach the BPF scheduler first so we can get the epoch_slots FD
    // before forking the child (child inherits the FD via fork).
    let mut open_object = MaybeUninit::uninit();
    let sched = Scheduler::init(&opts, &mut open_object)?;

    // Expose the epoch_slots FD to the child workload via environment variable.
    // The child reads this to find the BPF map without needing bpf_obj_get.
    if sched.epoch_slots_fd >= 0 {
        // SAFETY: single-threaded before forking child; no data race.
        unsafe {
            std::env::set_var("ULOCK_EPOCH_SLOTS_FD", sched.epoch_slots_fd.to_string());
            std::env::set_var("ULOCK_EPOCH_MS", opts.epoch_ms.to_string());
        }
    }

    // Optionally fork the child workload.
    let child: Option<ChildProcess> = if !opts.command.is_empty() {
        let c = ChildProcess::spawn_suspended(&opts.command)?;
        info!("child process {} suspended, loading scheduler", c.pid);
        Some(c)
    } else {
        None
    };

    // Resume the child after the scheduler is attached.
    if let Some(ref child) = child {
        child.resume()?;
        info!("child process {} resumed", child.pid);
    }

    // Controller main loop.
    let mut ctrl = Controller::new(&opts, sched.epoch_slots_fd);
    let period = ctrl.control_period;
    let mut next_tick = Instant::now() + period;

    loop {
        // Check shutdown signal.
        if shutdown.load(Ordering::Relaxed) {
            info!("shutdown requested");
            break;
        }

        // Check if the child workload has finished.
        if let Some(ref child) = child {
            if child.has_exited() {
                info!("child process finished, stopping scheduler");
                break;
            }
        }

        // Sleep until the next control period.
        let now = Instant::now();
        if now < next_tick {
            std::thread::sleep(next_tick - now);
        }
        next_tick += period;

        ctrl.tick();
    }

    info!("{SCHEDULER_NAME} exiting cleanly");
    Ok(())
}
