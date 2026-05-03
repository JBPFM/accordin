// SPDX-License-Identifier: GPL-2.0-only

type AccordinBackend = accordin_shared::mutex_hook::LockBackendAdapter<crate::AccordinRawLock>;

accordin_shared::export_mutex_hooks!(super::AccordinBackend);
