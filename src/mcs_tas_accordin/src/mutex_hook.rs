// SPDX-License-Identifier: GPL-2.0-only

use crate::mcs_tas::McsTasLockRaw;

type McsTasBackend = accordin_shared::mutex_hook::LockBackendAdapter<McsTasLockRaw>;

accordin_shared::export_mutex_hooks!(super::McsTasBackend);
