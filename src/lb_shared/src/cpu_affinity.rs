use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub const CPU_MASK_K_ENV: &str = "ACCORDIN_CPU_MASK_K";
pub const CPU_MASK_K_SHORT_ENV: &str = "K";

#[derive(Debug)]
struct CpuAffinityConfig {
    env_name: &'static str,
    requested_cpus: usize,
    available_cpus: Vec<usize>,
    active_cpu_count: AtomicUsize,
    generation: AtomicU64,
}

#[derive(Debug)]
enum CpuAffinityState {
    Disabled,
    Configured(CpuAffinityConfig),
    Invalid(String),
}

static CPU_AFFINITY_STATE: OnceLock<CpuAffinityState> = OnceLock::new();

thread_local! {
    static CURRENT_THREAD_AFFINITY_GENERATION: Cell<u64> = const { Cell::new(0) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DynamicCpuAffinityUpdate {
    pub requested_cpus: usize,
    pub applied_cpus: usize,
    pub changed: bool,
}

pub fn init_from_env(label: &str) {
    match cpu_affinity_state() {
        CpuAffinityState::Disabled => {}
        CpuAffinityState::Invalid(error) => {
            eprintln!("[{label}] CPU affinity env ignored: {error}");
        }
        CpuAffinityState::Configured(config) => match apply_current_configured_affinity(config) {
            Ok(()) => {
                eprintln!(
                    "[{label}] CPU affinity initially limited by {}={} to CPUs: {}",
                    config.env_name,
                    config.requested_cpus,
                    format_cpu_list(&active_cpus(config))
                );
            }
            Err(error) => {
                eprintln!(
                    "[{label}] failed to apply CPU affinity from {}={}: {error}",
                    config.env_name, config.requested_cpus
                );
            }
        },
    }
}

pub fn ensure_current_thread_affinity() {
    if let CpuAffinityState::Configured(config) = cpu_affinity_state() {
        let _ = apply_current_thread_affinity_if_needed(config);
    }
}

pub fn configured_cpu_count_env_present() -> bool {
    configured_cpu_count_env().is_some()
}

pub fn update_dynamic_cpu_count(
    requested_cpus: usize,
) -> Result<Option<DynamicCpuAffinityUpdate>, String> {
    let requested_cpus = requested_cpus.max(1);
    let config = match cpu_affinity_state() {
        CpuAffinityState::Disabled => return Ok(None),
        CpuAffinityState::Invalid(error) => return Err(error.clone()),
        CpuAffinityState::Configured(config) => config,
    };

    let applied_cpus = requested_cpus.min(config.available_cpus.len()).max(1);
    let previous_cpus = config.active_cpu_count.swap(applied_cpus, Ordering::AcqRel);
    let changed = previous_cpus != applied_cpus;
    if changed {
        config.generation.fetch_add(1, Ordering::AcqRel);
        apply_process_configured_affinity(config)?;
    }

    Ok(Some(DynamicCpuAffinityUpdate {
        requested_cpus,
        applied_cpus,
        changed,
    }))
}

pub fn current_dynamic_cpu_count() -> Option<usize> {
    match cpu_affinity_state() {
        CpuAffinityState::Configured(config) => {
            Some(config.active_cpu_count.load(Ordering::Acquire))
        }
        _ => None,
    }
}

fn cpu_affinity_state() -> &'static CpuAffinityState {
    CPU_AFFINITY_STATE.get_or_init(load_cpu_affinity_state)
}

fn load_cpu_affinity_state() -> CpuAffinityState {
    let Some((env_name, value)) = configured_cpu_count_env() else {
        return CpuAffinityState::Disabled;
    };

    let requested_cpus = match parse_requested_cpu_count(&value, env_name) {
        Ok(requested_cpus) => requested_cpus,
        Err(error) => return CpuAffinityState::Invalid(error),
    };

    let numa_cpus = match numa_ordered_cpus() {
        Ok(cpus) => cpus,
        Err(error) => return CpuAffinityState::Invalid(error),
    };
    let current_allowed_cpus = current_affinity_cpus().ok();
    let available_cpus = select_numa_available_cpus(&numa_cpus, current_allowed_cpus.as_deref());
    if available_cpus.len() < requested_cpus {
        return CpuAffinityState::Invalid(format!(
            "requested {requested_cpus} CPUs from NUMA-ordered available CPUs but only {} are available",
            available_cpus.len()
        ));
    }

    CpuAffinityState::Configured(CpuAffinityConfig {
        env_name,
        requested_cpus,
        available_cpus,
        active_cpu_count: AtomicUsize::new(requested_cpus),
        generation: AtomicU64::new(1),
    })
}

fn configured_cpu_count_env() -> Option<(&'static str, String)> {
    match std::env::var(CPU_MASK_K_ENV) {
        Ok(value) => Some((CPU_MASK_K_ENV, value)),
        Err(std::env::VarError::NotPresent) => match std::env::var(CPU_MASK_K_SHORT_ENV) {
            Ok(value) => Some((CPU_MASK_K_SHORT_ENV, value)),
            Err(_) => None,
        },
        Err(error) => Some((CPU_MASK_K_ENV, error.to_string())),
    }
}

fn parse_requested_cpu_count(value: &str, env_name: &str) -> Result<usize, String> {
    let trimmed = value.trim();
    let requested_cpus = trimmed
        .parse::<usize>()
        .map_err(|_| format!("{env_name} must be a positive integer, got {value:?}"))?;
    if requested_cpus == 0 {
        return Err(format!("{env_name} must be > 0"));
    }
    Ok(requested_cpus)
}

fn numa_ordered_cpus() -> Result<Vec<usize>, String> {
    let mut node_cpulists = numa_node_cpulist_paths("/sys/devices/system/node");
    node_cpulists.sort_by_key(|(node_id, _)| *node_id);

    let mut cpus = Vec::new();
    let mut seen = BTreeSet::new();
    for (_node_id, cpulist_path) in node_cpulists {
        for cpu in read_cpu_list_file(&cpulist_path)? {
            if seen.insert(cpu) {
                cpus.push(cpu);
            }
        }
    }

    if !cpus.is_empty() {
        return Ok(cpus);
    }

    read_cpu_list_file(Path::new("/sys/devices/system/cpu/online"))
        .map_err(|error| format!("failed to find NUMA node CPUs: {error}"))
}

fn numa_node_cpulist_paths(root: impl AsRef<Path>) -> Vec<(u32, PathBuf)> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };

    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let file_name = entry.file_name();
            let file_name = file_name.to_str()?;
            let node_id = file_name.strip_prefix("node")?.parse::<u32>().ok()?;
            Some((node_id, entry.path().join("cpulist")))
        })
        .collect()
}

fn read_cpu_list_file(path: &Path) -> Result<Vec<usize>, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    parse_cpu_list(&contents)
        .map_err(|error| format!("invalid CPU list in {}: {error}", path.display()))
}

fn parse_cpu_list(text: &str) -> Result<Vec<usize>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let mut cpus = BTreeSet::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("empty entry in {text:?}"));
        }

        if let Some((start, end)) = part.split_once('-') {
            let start = parse_cpu_id(start, text)?;
            let end = parse_cpu_id(end, text)?;
            if start > end {
                return Err(format!("invalid descending range {part:?}"));
            }
            for cpu in start..=end {
                cpus.insert(cpu);
            }
        } else {
            cpus.insert(parse_cpu_id(part, text)?);
        }
    }

    Ok(cpus.into_iter().collect())
}

fn parse_cpu_id(value: &str, source: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| format!("invalid CPU id {value:?} in {source:?}"))
}

fn current_affinity_cpus() -> io::Result<Vec<usize>> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let ret =
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut cpus = Vec::new();
    for cpu in 0..libc::CPU_SETSIZE as usize {
        if unsafe { libc::CPU_ISSET(cpu, &set) } {
            cpus.push(cpu);
        }
    }
    Ok(cpus)
}

#[cfg(test)]
fn select_numa_cpus(
    numa_cpus: &[usize],
    allowed_cpus: Option<&[usize]>,
    requested_cpus: usize,
) -> Result<Vec<usize>, String> {
    let cpus = select_numa_available_cpus(numa_cpus, allowed_cpus)
        .into_iter()
        .take(requested_cpus)
        .collect::<Vec<_>>();

    if cpus.len() < requested_cpus {
        return Err(format!(
            "requested {requested_cpus} CPUs from NUMA-ordered available CPUs but only {} are available",
            cpus.len()
        ));
    }

    Ok(cpus)
}

fn select_numa_available_cpus(numa_cpus: &[usize], allowed_cpus: Option<&[usize]>) -> Vec<usize> {
    let allowed: Option<BTreeSet<usize>> = allowed_cpus.map(|cpus| cpus.iter().copied().collect());
    numa_cpus
        .iter()
        .copied()
        .filter(|cpu| allowed.as_ref().is_none_or(|allowed| allowed.contains(cpu)))
        .collect()
}

fn active_cpus(config: &CpuAffinityConfig) -> Vec<usize> {
    let active_cpu_count = config.active_cpu_count.load(Ordering::Acquire);
    config
        .available_cpus
        .iter()
        .copied()
        .take(active_cpu_count)
        .collect()
}

fn apply_current_configured_affinity(config: &CpuAffinityConfig) -> Result<(), String> {
    apply_cpu_affinity(&active_cpus(config))
}

fn apply_process_configured_affinity(config: &CpuAffinityConfig) -> Result<(), String> {
    apply_process_affinity(&active_cpus(config))
}

fn apply_current_thread_affinity_if_needed(config: &CpuAffinityConfig) -> Result<bool, String> {
    let generation = config.generation.load(Ordering::Acquire);
    if generation == 0 {
        return Ok(false);
    }

    CURRENT_THREAD_AFFINITY_GENERATION.with(|applied_generation| {
        if applied_generation.get() == generation {
            return Ok(false);
        }

        let result = apply_current_configured_affinity(config);
        applied_generation.set(generation);
        result.map(|()| true)
    })
}

fn apply_cpu_affinity(cpus: &[usize]) -> Result<(), String> {
    apply_cpu_affinity_to_tid(0, cpus).map_err(|error| error.to_string())
}

fn apply_process_affinity(cpus: &[usize]) -> Result<(), String> {
    for tid in current_process_tids()? {
        match apply_cpu_affinity_to_tid(tid, cpus) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(())
}

fn current_process_tids() -> Result<Vec<libc::pid_t>, String> {
    let entries = fs::read_dir("/proc/self/task")
        .map_err(|error| format!("failed to read /proc/self/task: {error}"))?;
    let mut tids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("failed to read task entry: {error}"))?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Ok(tid) = file_name.parse::<libc::pid_t>() else {
            continue;
        };
        tids.push(tid);
    }
    Ok(tids)
}

fn apply_cpu_affinity_to_tid(tid: libc::pid_t, cpus: &[usize]) -> io::Result<()> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::CPU_ZERO(&mut set);
    }
    for &cpu in cpus {
        if cpu >= libc::CPU_SETSIZE as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "CPU {cpu} is outside libc CPU_SETSIZE {}",
                    libc::CPU_SETSIZE
                ),
            ));
        }
        unsafe {
            libc::CPU_SET(cpu, &mut set);
        }
    }

    let ret = unsafe { libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &set) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn format_cpu_list(cpus: &[usize]) -> String {
    cpus.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::{format_cpu_list, parse_cpu_list, select_numa_available_cpus, select_numa_cpus};

    #[test]
    fn parse_cpu_list_accepts_singletons_and_ranges() {
        assert_eq!(
            parse_cpu_list("0,2,4-6,10\n").unwrap(),
            vec![0, 2, 4, 5, 6, 10]
        );
    }

    #[test]
    fn parse_cpu_list_deduplicates_and_sorts() {
        assert_eq!(parse_cpu_list("4,2,2,0-1").unwrap(), vec![0, 1, 2, 4]);
    }

    #[test]
    fn parse_cpu_list_rejects_descending_ranges() {
        assert!(parse_cpu_list("4-2").unwrap_err().contains("descending"));
    }

    #[test]
    fn select_numa_cpus_respects_current_affinity() {
        let numa_cpus = vec![0, 2, 4, 6, 8, 10, 1, 3, 5, 7, 9, 11];
        let allowed_cpus = vec![2, 6, 8, 1, 3, 12];

        assert_eq!(
            select_numa_cpus(&numa_cpus, Some(&allowed_cpus), 5).unwrap(),
            vec![2, 6, 8, 1, 3]
        );
    }

    #[test]
    fn select_numa_cpus_requires_enough_available_cpus() {
        let error = select_numa_cpus(&[0, 2], Some(&[2]), 2).unwrap_err();
        assert!(error.contains("requested 2 CPUs"));
    }

    #[test]
    fn select_numa_available_cpus_keeps_full_pool_across_nodes() {
        let numa_cpus = vec![0, 2, 4, 6, 8, 10, 1, 3, 5, 7, 9, 11];
        let allowed_cpus = vec![2, 6, 8, 1, 3, 12];

        assert_eq!(
            select_numa_available_cpus(&numa_cpus, Some(&allowed_cpus)),
            vec![2, 6, 8, 1, 3]
        );
    }

    #[test]
    fn select_numa_cpus_spills_to_later_nodes_after_earlier_nodes() {
        let numa_cpus = vec![0, 2, 4, 1, 3, 5];

        assert_eq!(
            select_numa_cpus(&numa_cpus, None, 5).unwrap(),
            vec![0, 2, 4, 1, 3]
        );
    }

    #[test]
    fn format_cpu_list_joins_selected_cpus() {
        assert_eq!(format_cpu_list(&[0, 2, 4]), "0,2,4");
    }
}
