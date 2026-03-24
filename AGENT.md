## BPF Owner-Preemption Constraints

- 在 `sched_ext` / `struct_ops` 这条路径里，不允许使用 `bpf_probe_write_user()` 回写用户态 owner 状态；当前内核会直接被 verifier 拒绝。
- owner-preemption 需要改成 BPF 持有的共享状态：由 BPF map 注册状态变量，用户态通过 map 的 mmap 地址拿到对应指针并读取。
