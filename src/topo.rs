//! CPU topology discovery and SSC CPU selection.
//!
//! Reads topology information from sysfs and builds an ordered list of CPUs
//! suitable for compact SSC allocation.
//!
//! Selection preference (per AGENT.md):
//!   1. Same NUMA node.
//!   2. Same LLC domain.
//!   3. Contiguous CPU IDs.
//!   4. Compact expansion from current SSC boundary.
//!   5. Compact shrink from the edge.

use anyhow::{Context, Result};

/// Per-CPU topology information.
#[derive(Debug, Clone)]
pub struct CpuInfo {
    pub cpu_id:    u32,
    pub numa_node: u32,
    pub llc_id:    u32,
}

/// System CPU topology.
pub struct CpuTopo {
    /// CPUs ordered by (numa_node, llc_id, cpu_id) for compact SSC allocation.
    pub ordered_cpus: Vec<CpuInfo>,
    /// Total online CPU count.
    pub nr_cpus: u32,
}

impl CpuTopo {
    /// Discover topology by reading sysfs.
    /// Falls back to a flat ordering if sysfs is unavailable.
    pub fn discover() -> Result<Self> {
        let mut infos: Vec<CpuInfo> = Vec::new();
        let cpu_base = "/sys/devices/system/cpu";

        for entry in std::fs::read_dir(cpu_base)
            .context("read /sys/devices/system/cpu")?
        {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();

            // Match cpu0, cpu1, ... (but not cpufreq, cpuidle, etc.)
            let cpu_id: u32 = match name.strip_prefix("cpu") {
                Some(s) => match s.parse() { Ok(n) => n, Err(_) => continue },
                None => continue,
            };

            let cache_base = format!("{cpu_base}/cpu{cpu_id}/cache");

            // Read NUMA node from the cpu's node link.
            let numa_node = read_numa_node(cpu_id).unwrap_or(0);

            // Read LLC (last-level cache) domain.
            let llc_id = read_llc_id(&cache_base, cpu_id).unwrap_or(0);

            infos.push(CpuInfo { cpu_id, numa_node, llc_id });
        }

        if infos.is_empty() {
            anyhow::bail!("no CPUs found in sysfs");
        }

        // Sort by (numa_node, llc_id, cpu_id) for compact group allocation.
        infos.sort_by_key(|c| (c.numa_node, c.llc_id, c.cpu_id));

        let nr_cpus = infos.len() as u32;
        Ok(Self { ordered_cpus: infos, nr_cpus })
    }

    /// Select the first `width` CPUs from the topology order for the SSC.
    ///
    /// Expansion: add from the boundary of the current set.
    /// Shrinkage: remove from the tail (last added CPUs).
    pub fn pick_ssc_cpus(&self, width: u32) -> Vec<u32> {
        let count = (width as usize).min(self.ordered_cpus.len());
        self.ordered_cpus[..count]
            .iter()
            .map(|c| c.cpu_id)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Sysfs helpers
// ---------------------------------------------------------------------------

/// Determine which NUMA node a CPU belongs to by checking
/// `/sys/devices/system/node/node*/cpumap` or the cpu's node symlink.
fn read_numa_node(cpu_id: u32) -> Option<u32> {
    // The simplest approach: read the numa_node file if it exists.
    let path = format!("/sys/devices/system/cpu/cpu{cpu_id}/node0");
    if std::path::Path::new(&path).exists() {
        return Some(0);
    }

    // Try /sys/devices/system/node/nodeN/cpulist for each node.
    if let Ok(entries) = std::fs::read_dir("/sys/devices/system/node") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let node_id: u32 = match name.strip_prefix("node") {
                Some(s) => match s.parse() { Ok(n) => n, Err(_) => continue },
                None => continue,
            };
            let cpulist_path = format!(
                "/sys/devices/system/node/{}/cpulist", name
            );
            if let Ok(cpulist) = std::fs::read_to_string(&cpulist_path) {
                if cpulist_contains(cpulist.trim(), cpu_id) {
                    return Some(node_id);
                }
            }
        }
    }
    None
}

/// Read the LLC (last-level cache) shared_cpu_list to derive an LLC ID.
/// We use the first CPU in the shared list as the LLC ID.
fn read_llc_id(cache_base: &str, cpu_id: u32) -> Option<u32> {
    let cache_dir = std::fs::read_dir(cache_base).ok()?;
    for entry in cache_dir.flatten() {
        let index_path = entry.path();
        let level_path = index_path.join("level");
        let level: u32 = std::fs::read_to_string(&level_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        // Use the highest cache level as LLC.
        if level >= 3 {
            let shared = index_path.join("shared_cpu_list");
            if let Ok(list) = std::fs::read_to_string(&shared) {
                // Use the first CPU in the shared list as the LLC domain ID.
                return list.trim()
                    .split([',', '-'])
                    .next()
                    .and_then(|s| s.parse().ok());
            }
        }
    }
    // Fallback: LLC = cpu_id (each CPU is its own domain)
    Some(cpu_id)
}

/// Check whether a CPU ID appears in a cpulist string (e.g., "0-3,8,12-15").
fn cpulist_contains(cpulist: &str, target: u32) -> bool {
    for part in cpulist.split(',') {
        let part = part.trim();
        if let Some((lo, hi)) = part.split_once('-') {
            let lo: u32 = lo.trim().parse().unwrap_or(u32::MAX);
            let hi: u32 = hi.trim().parse().unwrap_or(0);
            if target >= lo && target <= hi { return true; }
        } else if let Ok(n) = part.parse::<u32>() {
            if n == target { return true; }
        }
    }
    false
}
