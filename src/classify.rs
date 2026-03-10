//! Task classification state machine.
//!
//! Each task transitions between three classes based on its lock wait ratio
//! in a sliding window of epochs.  All thresholds are configurable.

/// Task class constants (must match `enum task_class` in intf.h).
pub const TASK_NORMAL:         u32 = 0;
pub const TASK_CANDIDATE:      u32 = 1;
pub const TASK_LOCK_INTENSIVE: u32 = 2;

/// Per-task classification state maintained in the controller.
#[derive(Debug, Clone)]
pub struct TaskState {
    pub pid:         u32,
    pub tgid:        u32,
    pub cls:         u32,
    pub hot_epochs:  u32,
    pub cool_epochs: u32,
    /// Last epoch_id processed; avoids double-counting the same epoch.
    pub last_epoch:  u32,
}

impl TaskState {
    pub fn new(pid: u32, tgid: u32) -> Self {
        Self {
            pid,
            tgid,
            cls:         TASK_NORMAL,
            hot_epochs:  0,
            cool_epochs: 0,
            last_epoch:  0,
        }
    }
}

/// Classification parameters (mirrors the BPF config fields).
pub struct ClassifyConfig {
    pub enter_threshold_pct: u32,
    pub exit_threshold_pct:  u32,
    pub min_contended_acq:   u64,
    pub hot_epochs_needed:   u32,
    pub cool_epochs_needed:  u32,
}

/// Run one classification step for a single task.
///
/// `epoch_runtime_ns` is the on-CPU time for this task in the current epoch
/// (from BPF `task_ctx.run_ns`).  If zero, no scheduling activity occurred
/// this epoch and the class is left unchanged.
///
/// Returns the new class (may be unchanged).
pub fn classify_task(
    state:            &mut TaskState,
    wait_ns:          u64,
    epoch_runtime_ns: u64,
    contended_acq:    u64,
    epoch_id:         u32,
    cfg:              &ClassifyConfig,
) -> u32 {
    // Skip epochs with no runtime signal or already processed.
    if epoch_runtime_ns == 0 || epoch_id == state.last_epoch {
        return state.cls;
    }
    state.last_epoch = epoch_id;

    let wait_ratio_pct = (wait_ns * 100).saturating_div(epoch_runtime_ns);

    match state.cls {
        TASK_NORMAL | TASK_CANDIDATE => {
            if wait_ratio_pct >= cfg.enter_threshold_pct as u64
                && contended_acq >= cfg.min_contended_acq
            {
                state.hot_epochs += 1;
                state.cool_epochs = 0;
                if state.hot_epochs >= cfg.hot_epochs_needed {
                    state.cls = TASK_LOCK_INTENSIVE;
                } else {
                    state.cls = TASK_CANDIDATE;
                }
            } else {
                state.hot_epochs = 0;
                state.cls = TASK_NORMAL;
            }
        }
        TASK_LOCK_INTENSIVE => {
            if wait_ratio_pct <= cfg.exit_threshold_pct as u64 {
                state.cool_epochs += 1;
                state.hot_epochs = 0;
                if state.cool_epochs >= cfg.cool_epochs_needed {
                    state.cls = TASK_NORMAL;
                }
            } else {
                state.cool_epochs = 0;
            }
        }
        _ => {}
    }

    state.cls
}
