//! Online SSC width search.
//!
//! Uses a normalized-throughput proxy to decide whether to grow or shrink the
//! SSC (Special Set of Cores):
//!
//!   p             = voting_lock_ns / voting_slice_ns
//!   work_cores    = min(ssc_width, nr_lock_intensive_tasks)
//!   proxy         = work_cores * (1.0 - p)
//!
//! The proxy increases when adding more SSC cores improves effective
//! throughput.  It decreases when the lock-wait ratio is so high that extra
//! cores don't help.
//!
//! Search policy:
//!   - 2 consecutive improvements → try doubling ssc_width (capped at max).
//!   - 2 consecutive degradations → step back by 1 (floor at 1).
//!   - Reset on significant workload-behaviour change (wait_ratio swing > 20%).

/// Aggregate metrics consumed by the search algorithm each control period.
#[derive(Debug, Clone, Default)]
pub struct AggMetrics {
    /// Total lock-wait nanoseconds across all lock-intensive tasks this period.
    pub voting_lock_ns: u64,
    /// Total scheduling-slice nanoseconds for lock-intensive tasks this period.
    pub voting_slice_ns: u64,
    /// Number of tasks currently classified as LOCK_INTENSIVE.
    pub nr_lock_intensive: u32,
}

/// Online SSC width searcher.
pub struct SscSearch {
    ssc_width:      u32,
    max_ssc_width:  u32,
    prev_proxy:     f64,
    improve_count:  u32,
    degrade_count:  u32,
    prev_p:         f64,
}

impl SscSearch {
    pub fn new(max_ssc_width: u32) -> Self {
        Self {
            ssc_width:     1,
            max_ssc_width,
            prev_proxy:    0.0,
            improve_count: 0,
            degrade_count: 0,
            prev_p:        0.0,
        }
    }

    /// Current SSC width.
    pub fn width(&self) -> u32 { self.ssc_width }

    /// Run one search step with the provided aggregate metrics.
    ///
    /// Returns `Some(new_width)` if the SSC width changed, `None` otherwise.
    pub fn step(&mut self, metrics: &AggMetrics) -> Option<u32> {
        if metrics.voting_slice_ns == 0 || metrics.nr_lock_intensive == 0 {
            return None;
        }

        let p = (metrics.voting_lock_ns as f64)
            / (metrics.voting_slice_ns as f64).max(1.0);
        let p = p.clamp(0.0, 1.0);

        // Detect significant workload change (p swing > 20%) → reset search.
        if (p - self.prev_p).abs() > 0.20 {
            self.improve_count = 0;
            self.degrade_count = 0;
            self.prev_p = p;
            self.prev_proxy = 0.0;
            return None;
        }
        self.prev_p = p;

        let work_cores = (self.ssc_width as f64)
            .min(metrics.nr_lock_intensive as f64);
        let proxy = work_cores * (1.0 - p);

        let changed = if proxy > self.prev_proxy {
            self.improve_count += 1;
            self.degrade_count = 0;
            if self.improve_count >= 2 {
                self.improve_count = 0;
                let new_width = (self.ssc_width * 2).min(self.max_ssc_width);
                if new_width != self.ssc_width {
                    self.ssc_width = new_width;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            self.degrade_count += 1;
            self.improve_count = 0;
            if self.degrade_count >= 2 && self.ssc_width > 1 {
                self.degrade_count = 0;
                self.ssc_width -= 1;
                true
            } else {
                false
            }
        };

        self.prev_proxy = proxy;

        if changed { Some(self.ssc_width) } else { None }
    }
}
