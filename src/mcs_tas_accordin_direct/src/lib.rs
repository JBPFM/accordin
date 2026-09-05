// SPDX-License-Identifier: GPL-2.0-only

mod bpf_skel;
pub use bpf_skel::*;

pub use accordin_shared::admission;
pub use accordin_shared::arch;
pub use accordin_shared::env::env_flag;
pub use accordin_shared::lock_backend;

#[allow(non_camel_case_types, non_upper_case_globals, dead_code)]
pub mod bpf_intf {
    include!(concat!(env!("OUT_DIR"), "/bpf_intf.rs"));
}

mod direct_lock;
mod mcs_tas;

accordin_shared::define_scheduler_loader!(
    scheduler_name = "mcs_tas_accordin_direct",
    env_prefix = "MCS_TAS_ACCORDIN_DIRECT",
);
