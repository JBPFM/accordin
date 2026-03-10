// SPDX-License-Identifier: GPL-2.0-only
//
// 动态链接库入口，在加载时初始化 eBPF 调度器

mod bpf_skel;
pub use bpf_skel::*;
mod arch;
pub mod bpf_intf;
mod mcs_tas;
mod mutex_hook;

use std::mem::MaybeUninit;
use std::sync::OnceLock;

use anyhow::Result;
use libbpf_rs::Link;
use libbpf_rs::OpenObject;
use log::info;
use scx_utils::scx_ops_attach;
use scx_utils::scx_ops_load;
use scx_utils::scx_ops_open;

const SCHEDULER_NAME: &str = "lb_simple";

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

fn init_scheduler(debug: bool) -> Result<SchedulerState> {
    let mut skel_builder = BpfSkelBuilder::default();
    skel_builder.obj_builder.debug(debug);

    // 使用 Box::leak 来保持 OpenObject 的生命周期
    let open_object: &'static mut MaybeUninit<OpenObject> =
        Box::leak(Box::new(MaybeUninit::uninit()));

    // Open the BPF skeleton
    let mut skel = scx_ops_open!(skel_builder, open_object, lb_simple_ops, None)?;

    // Load the BPF program
    let mut skel = scx_ops_load!(skel, lb_simple_ops, uei)?;

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

    // 初始化调度器（只执行一次）
    let _ = SCHEDULER_STATE.get_or_init(|| match init_scheduler(false) {
        Ok(state) => {
            eprintln!("[lb_simple] eBPF scheduler loaded successfully");
            state
        }
        Err(e) => {
            eprintln!("[lb_simple] Failed to load eBPF scheduler: {}", e);
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
