pub use accordin_shared::admission;
pub use accordin_shared::arch;
pub use accordin_shared::lock_backend;
pub use accordin_shared::lock_stats;
pub use accordin_shared::timeslice_extension;

mod mcs;
mod mutex_hook;

use accordin_shared::timeslice_extension::{
    TimesliceExtensionMode, current_thread_timeslice_extension_status,
};
use log::info;

const LOCK_NAME: &str = "mcs_tse";

fn init_tse() {
    if cfg!(test) {
        return;
    }

    let _ = simplelog::TermLogger::init(
        simplelog::LevelFilter::Info,
        simplelog::Config::default(),
        simplelog::TerminalMode::Stderr,
        simplelog::ColorChoice::Auto,
    );

    let status = current_thread_timeslice_extension_status(TimesliceExtensionMode::Require);
    if !status.enabled {
        let reason = status.reason.unwrap_or("unknown failure");
        if status.error_number != 0 {
            eprintln!(
                "[mcs_tse] timeslice extension unavailable: {} (errno={})",
                reason, status.error_number
            );
        } else {
            eprintln!("[mcs_tse] timeslice extension unavailable: {}", reason);
        }
        panic!("timeslice extension initialization failed");
    }

    info!("{LOCK_NAME} lock interposer loaded via LD_PRELOAD");
    eprintln!("[mcs_tse] mutexbench TSE loaded successfully");
}

#[unsafe(link_section = ".init_array")]
#[used]
static INIT: extern "C" fn() = {
    extern "C" fn init() {
        init_tse();
    }
    init
};

#[unsafe(link_section = ".fini_array")]
#[used]
static FINI: extern "C" fn() = {
    extern "C" fn fini() {
        lock_stats::print_process_stats("mcs_tse");
    }
    fini
};
