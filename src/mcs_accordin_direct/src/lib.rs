// SPDX-License-Identifier: GPL-2.0-only

mod bpf_skel;
pub use bpf_skel::*;

#[allow(non_camel_case_types, non_upper_case_globals, dead_code)]
pub mod bpf_intf {
    include!(concat!(env!("OUT_DIR"), "/bpf_intf.rs"));
}

mod direct_lock;
mod mcs;

accordin_shared::define_scheduler_loader!(
    scheduler_name = "mcs_accordin_direct",
    env_prefix = "MCS_ACCORDIN_DIRECT",
);
