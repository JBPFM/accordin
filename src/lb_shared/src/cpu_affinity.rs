use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const CPU_MASK_K_ENV: &str = "LB_SIMPLE_CPU_MASK_K";
pub const CPU_MASK_K_SHORT_ENV: &str = "K";

#[derive(Debug)]
struct CpuAffinityConfig {
    env_name: &'static str,
    requested_cpus: usize,
    cpus: Vec<usize>,
}

#[derive(Debug)]
enum CpuAffinityState {
    Disabled,
    Configured(CpuAffinityConfig),
    Invalid(String),
}

static CPU_AFFINITY_STATE: OnceLock<CpuAffinityState> = OnceLock::new();

thread_local! {
    static CURRENT_THREAD_AFFINITY_APPLIED: Cell<bool> = const { Cell::new(false) };
}

pub fn init_from_env(label: &str) {
    match cpu_affinity_state() {
        CpuAffinityState::Disabled => {}
        CpuAffinityState::Invalid(error) => {
            eprintln!("[{label}] CPU affinity env ignored: {error}");
        }
        CpuAffinityState::Configured(config) => match apply_cpu_affinity(&config.cpus) {
            Ok(()) => {
                eprintln!(
                    "[{label}] CPU affinity limited by {}={} to CPUs: {}",
                    config.env_name,
                    config.requested_cpus,
                    format_cpu_list(&config.cpus)
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
        let _ = apply_current_thread_affinity_once(config);
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

    let first_node_cpus = match first_numa_node_cpus() {
        Ok(cpus) => cpus,
        Err(error) => return CpuAffinityState::Invalid(error),
    };
    let current_allowed_cpus = current_affinity_cpus().ok();
    let cpus = match select_first_numa_cpus(
        &first_node_cpus,
        current_allowed_cpus.as_deref(),
        requested_cpus,
    ) {
        Ok(cpus) => cpus,
        Err(error) => return CpuAffinityState::Invalid(error),
    };

    CpuAffinityState::Configured(CpuAffinityConfig {
        env_name,
        requested_cpus,
        cpus,
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

fn first_numa_node_cpus() -> Result<Vec<usize>, String> {
    let mut node_cpulists = numa_node_cpulist_paths("/sys/devices/system/node");
    node_cpulists.sort_by_key(|(node_id, _)| *node_id);

    for (_node_id, cpulist_path) in node_cpulists {
        let cpus = read_cpu_list_file(&cpulist_path)?;
        if !cpus.is_empty() {
            return Ok(cpus);
        }
    }

    read_cpu_list_file(Path::new("/sys/devices/system/cpu/online"))
        .map_err(|error| format!("failed to find first NUMA node CPUs: {error}"))
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

fn select_first_numa_cpus(
    numa_cpus: &[usize],
    allowed_cpus: Option<&[usize]>,
    requested_cpus: usize,
) -> Result<Vec<usize>, String> {
    let allowed: Option<BTreeSet<usize>> = allowed_cpus.map(|cpus| cpus.iter().copied().collect());
    let cpus: Vec<usize> = numa_cpus
        .iter()
        .copied()
        .filter(|cpu| allowed.as_ref().is_none_or(|allowed| allowed.contains(cpu)))
        .take(requested_cpus)
        .collect();

    if cpus.len() < requested_cpus {
        return Err(format!(
            "requested {requested_cpus} CPUs from the first NUMA node but only {} are available",
            cpus.len()
        ));
    }

    Ok(cpus)
}

fn apply_current_thread_affinity_once(config: &CpuAffinityConfig) -> Result<bool, String> {
    CURRENT_THREAD_AFFINITY_APPLIED.with(|applied| {
        if applied.get() {
            return Ok(false);
        }

        let result = apply_cpu_affinity(&config.cpus);
        applied.set(true);
        result.map(|()| true)
    })
}

fn apply_cpu_affinity(cpus: &[usize]) -> Result<(), String> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::CPU_ZERO(&mut set);
    }
    for &cpu in cpus {
        if cpu >= libc::CPU_SETSIZE as usize {
            return Err(format!(
                "CPU {cpu} is outside libc CPU_SETSIZE {}",
                libc::CPU_SETSIZE
            ));
        }
        unsafe {
            libc::CPU_SET(cpu, &mut set);
        }
    }

    let ret = unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) };
    if ret != 0 {
        return Err(io::Error::last_os_error().to_string());
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
    use super::{format_cpu_list, parse_cpu_list, select_first_numa_cpus};

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
    fn select_first_numa_cpus_respects_current_affinity() {
        let numa_cpus = vec![0, 2, 4, 6, 8, 10];
        let allowed_cpus = vec![2, 6, 8, 12];

        assert_eq!(
            select_first_numa_cpus(&numa_cpus, Some(&allowed_cpus), 3).unwrap(),
            vec![2, 6, 8]
        );
    }

    #[test]
    fn select_first_numa_cpus_requires_enough_available_cpus() {
        let error = select_first_numa_cpus(&[0, 2], Some(&[2]), 2).unwrap_err();
        assert!(error.contains("requested 2 CPUs"));
    }

    #[test]
    fn format_cpu_list_joins_selected_cpus() {
        assert_eq!(format_cpu_list(&[0, 2, 4]), "0,2,4");
    }
}
