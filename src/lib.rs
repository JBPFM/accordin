// SPDX-License-Identifier: GPL-2.0-only
//
// 动态链接库入口，在加载时初始化 eBPF 调度器

mod bpf_skel;
pub use bpf_skel::*;
mod arch;
pub mod bpf_intf;
mod lock_backend;
mod lock_stats;
mod mcs_tas;
mod mutex_hook;

use std::mem::MaybeUninit;
use std::sync::OnceLock;

use anyhow::Result;
use libbpf_rs::Link;
use libbpf_rs::MapCore;
use libbpf_rs::MapFlags;
use libbpf_rs::MapHandle;
use libbpf_rs::OpenObject;
use log::info;
use scx_utils::scx_ops_attach;
use scx_utils::scx_ops_load;
use scx_utils::scx_ops_open;

const SCHEDULER_NAME: &str = "lb_simple";
const DISABLE_BPF_ENV: &str = "LB_SIMPLE_DISABLE_BPF";
const STATS_ONLY_ENV: &str = "LB_SIMPLE_STATS_ONLY";
const DEBUG_COUNTERS_ENV: &str = "LB_SIMPLE_DEBUG_COUNTERS";
const SSC_CPU_CAP: usize = bpf_intf::MAX_CPUS as usize;

// 全局状态，保持 eBPF 程序和 OpenObject 的生命周期
static SCHEDULER_STATE: OnceLock<SchedulerState> = OnceLock::new();

struct SchedulerState {
    // Keep link and loaded skel alive for the entire process lifetime.
    _link: Option<Link>,
    _skel: Option<BpfSkel<'static>>,
}

// SAFETY: BpfSkel/Link are internally thread-safe for this usage.
unsafe impl Send for SchedulerState {}
unsafe impl Sync for SchedulerState {}

#[derive(Debug, Default)]
struct NumaTopology {
    cpu_to_node: Vec<(u32, u32)>,
    dominant_node: i32,
    local_cpu_count: i64,
    remote_cpu_count: i64,
    first_socket_node: i32,
    first_socket_cpus: Vec<u32>,
}

fn parse_cpu_list(cpulist: &str) -> Vec<u32> {
    let mut cpus = Vec::new();

    for range_str in cpulist.trim().split(',') {
        let range_str = range_str.trim();
        if range_str.is_empty() {
            continue;
        }

        if let Some((start_s, end_s)) = range_str.split_once('-') {
            let start: u32 = match start_s.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let end: u32 = match end_s.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            if start > end {
                continue;
            }
            for cpu in start..=end {
                cpus.push(cpu);
            }
        } else if let Ok(cpu) = range_str.parse::<u32>() {
            cpus.push(cpu);
        }
    }

    cpus
}

fn build_numa_topology_from_nodes(mut node_cpus: Vec<(u32, Vec<u32>)>) -> NumaTopology {
    node_cpus.retain(|(_, cpus)| !cpus.is_empty());
    node_cpus.sort_by_key(|(node_id, _)| *node_id);

    if node_cpus.is_empty() {
        let fallback = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1)
            .max(1);
        let fallback_cpus: Vec<u32> = (0..fallback).collect();

        return NumaTopology {
            cpu_to_node: fallback_cpus.iter().copied().map(|cpu| (cpu, 0)).collect(),
            dominant_node: 0,
            local_cpu_count: i64::from(fallback),
            remote_cpu_count: 0,
            first_socket_node: 0,
            first_socket_cpus: fallback_cpus,
        };
    }

    let first_socket_node = node_cpus[0].0;
    let first_socket_cpus = node_cpus[0].1.clone();
    let mut cpu_to_node = Vec::new();
    let mut node_counts = Vec::new();

    for (node_id, cpus) in node_cpus {
        node_counts.push((node_id, cpus.len() as i64));
        for cpu in cpus {
            cpu_to_node.push((cpu, node_id));
        }
    }

    node_counts.sort_by_key(|(node_id, count)| (-*count, *node_id as i64));
    let (dominant_node, local_cpu_count) = node_counts[0];
    let total_cpu_count: i64 = node_counts.iter().map(|(_, count)| *count).sum();

    NumaTopology {
        cpu_to_node,
        dominant_node: dominant_node as i32,
        local_cpu_count: local_cpu_count.max(1),
        remote_cpu_count: (total_cpu_count - local_cpu_count).max(0),
        first_socket_node: first_socket_node as i32,
        first_socket_cpus,
    }
}

fn detect_numa_topology() -> NumaTopology {
    let node_base = "/sys/devices/system/node";
    let entries = match std::fs::read_dir(node_base) {
        Ok(e) => e,
        Err(_) => return build_numa_topology_from_nodes(Vec::new()),
    };

    let mut node_cpus = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with("node") {
            continue;
        }
        let node_id: u32 = match name_str[4..].parse() {
            Ok(n) => n,
            Err(_) => continue,
        };

        let cpulist_path = format!("{}/{}/cpulist", node_base, name_str);
        let cpulist = match std::fs::read_to_string(&cpulist_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let cpus = parse_cpu_list(&cpulist);
        if cpus.is_empty() {
            continue;
        }

        node_cpus.push((node_id, cpus));
    }

    build_numa_topology_from_nodes(node_cpus)
}

fn configure_scheduler_topology(_skel: &mut OpenBpfSkel<'_>, _topology: &NumaTopology) {}

fn publish_ssc_cpu_topology(
    ssc_cpu_count: &mut u32,
    ssc_cpu_list: &mut [u32; SSC_CPU_CAP],
    ssc_cpu_rank: &mut [u16; SSC_CPU_CAP],
    topology: &NumaTopology,
) {
    ssc_cpu_list.fill(0);
    ssc_cpu_rank.fill(SSC_CPU_CAP as u16);

    let mut count = 0usize;
    for cpu in topology.first_socket_cpus.iter().copied() {
        let cpu_idx = cpu as usize;
        if cpu_idx >= SSC_CPU_CAP || count >= SSC_CPU_CAP {
            continue;
        }

        ssc_cpu_list[count] = cpu;
        ssc_cpu_rank[cpu_idx] = count as u16;
        count += 1;
    }
    *ssc_cpu_count = count as u32;
}

fn initial_ssc_active_count(ssc_cpu_count: u32) -> u32 {
    ssc_cpu_count.clamp(2, 8)
}

/// Populate cpu_to_node BPF map and publish NUMA defaults.
fn publish_scheduler_topology(
    skel: &mut BpfSkel<'_>,
    topology: &NumaTopology,
    stats_only: bool,
    debug_counters: bool,
) {
    for (cpu, node_id) in &topology.cpu_to_node {
        let _ =
            skel.maps
                .cpu_to_node
                .update(&cpu.to_ne_bytes(), &node_id.to_ne_bytes(), MapFlags::ANY);
    }

    if let Some(bss) = skel.maps.bss_data.as_mut() {
        bss.dominant_node = topology.dominant_node;
        bss.stats_only_mode = u32::from(stats_only);
        bss.dbg_counters_enabled = u32::from(debug_counters);
        publish_ssc_cpu_topology(
            &mut bss.ssc_cpu_count,
            &mut bss.ssc_cpu_list,
            &mut bss.ssc_cpu_rank,
            topology,
        );
    }

    if let Some(data) = skel.maps.data_data.as_mut() {
        data.ssc_active_count = initial_ssc_active_count(
            u32::try_from(topology.first_socket_cpus.len()).unwrap_or(u32::MAX),
        );
    }

    info!(
        "lb_simple topology initialized: dominant_node={} local_cpus={} remote_cpus={} first_socket_node={} ssc_cpu_count={} stats_only={}",
        topology.dominant_node,
        topology.local_cpu_count,
        topology.remote_cpu_count,
        topology.first_socket_node,
        topology.first_socket_cpus.len(),
        stats_only
    );
}

fn init_scheduler(debug: bool, stats_only: bool, debug_counters: bool) -> Result<SchedulerState> {
    let topology = detect_numa_topology();
    let mut skel_builder = BpfSkelBuilder::default();
    skel_builder.obj_builder.debug(debug);

    // 使用 Box::leak 来保持 OpenObject 的生命周期
    let open_object: &'static mut MaybeUninit<OpenObject> =
        Box::leak(Box::new(MaybeUninit::uninit()));

    // Open the BPF skeleton
    let mut skel = scx_ops_open!(skel_builder, open_object, lb_simple_ops, None)?;
    configure_scheduler_topology(&mut skel, &topology);

    // NOTE: SWITCH_PARTIAL is intentionally NOT set.
    // All tasks go through sched_ext so that BPF callbacks can still see every
    // runnable thread. Admission accounting inside BPF is restricted to threads
    // that registered thread_ctx_addr_map entries, so unrelated system tasks do
    // not dilute lb_simple's wait-ratio feedback loop.

    // Load the BPF program
    let mut skel = scx_ops_load!(skel, lb_simple_ops, uei)?;

    // Duplicate the map handle so mutex hooks can use libbpf helpers directly.
    let thread_ctx_map = MapHandle::try_from(&skel.maps.thread_ctx_addr_map)?;
    mutex_hook::set_thread_ctx_map(thread_ctx_map);

    // Publish cpu_to_node map and NUMA defaults after load.
    publish_scheduler_topology(&mut skel, &topology, stats_only, debug_counters);

    // Attach the scheduler
    let link = scx_ops_attach!(skel, lb_simple_ops)?;

    info!("{SCHEDULER_NAME} scheduler started via LD_PRELOAD");
    Ok(SchedulerState {
        _link: Some(link),
        _skel: Some(skel),
    })
}

impl Drop for SchedulerState {
    fn drop(&mut self) {
        // Drop link first, then skeleton.
        let _ = self._link.take();
        let _ = self._skel.take();

        info!("{SCHEDULER_NAME} scheduler stopped");
    }
}

fn env_flag(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim();
            value == "1"
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
                || value.eq_ignore_ascii_case("on")
        }
        Err(_) => false,
    }
}

/// 初始化 eBPF 调度器
fn init_ebpf() {
    if cfg!(test) {
        return;
    }

    // 初始化日志
    let _ = simplelog::TermLogger::init(
        simplelog::LevelFilter::Info,
        simplelog::Config::default(),
        simplelog::TerminalMode::Stderr,
        simplelog::ColorChoice::Auto,
    );

    if env_flag(DISABLE_BPF_ENV) {
        info!(
            "{SCHEDULER_NAME} scheduler disabled by env {}",
            DISABLE_BPF_ENV
        );
        eprintln!("[lb_simple] eBPF scheduler disabled by {}", DISABLE_BPF_ENV);
        return;
    }

    let stats_only = env_flag(STATS_ONLY_ENV);
    let debug_counters = env_flag(DEBUG_COUNTERS_ENV);

    // 初始化调度器（只执行一次）
    let _ =
        SCHEDULER_STATE.get_or_init(|| match init_scheduler(false, stats_only, debug_counters) {
            Ok(state) => {
                if stats_only {
                    info!(
                        "{SCHEDULER_NAME} scheduler running in stats-only mode via env {}",
                        STATS_ONLY_ENV
                    );
                    eprintln!(
                        "[lb_simple] eBPF scheduler stats-only mode enabled by {}",
                        STATS_ONLY_ENV
                    );
                }
                eprintln!("[lb_simple] eBPF scheduler loaded successfully");
                state
            }
            Err(e) => {
                eprintln!("[lb_simple] Failed to load eBPF scheduler: {:#}", e);
                panic!("eBPF initialization failed");
            }
        });
}

// 库加载时的构造函数
#[unsafe(link_section = ".init_array")]
#[used]
static INIT: extern "C" fn() = {
    extern "C" fn init() {
        init_ebpf();
    }
    init
};

#[cfg(test)]
mod tests {
    use super::*;

    fn compact(source: &str) -> String {
        source.split_whitespace().collect()
    }
}
