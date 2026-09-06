# 工作区合入验证

工作区分支：simplify；检查完整通过：True。生产改动仅为 src/bpf/main.bpf.c 的 accordin_tick；见 [最终补丁](production.patch)。

确认测量使用隔离构建；工作区的 88 个生产源码/头文件及 Makefile 哈希与 tick 候选一致。工作区另行重新构建并检查；不把不同路径的动态库声称为字节完全相同。详细哈希见 [检查元数据](integration-metadata.json)。

| 检查命令 | 返回码 |
| --- | ---: |
| `make -j8 check check-litl` | 0 |
| `make check-bpf check-litl-bpf` | 0 |
| `taskset -c 0,1 env LITL_TEST_THREADS=24 LITL_TEST_ITERATIONS=1000 make check-bpf check-litl-bpf` | 0 |

检查覆盖直接 C API、标准 LiTL C/C++、NDEBUG、无 shadow mutex、取消/超时及 24 线程/2 CPU 的超额订阅配置。完成时 sched_ext 为 disabled。构建与检查日志位于 logs.tar.gz 的 integration-check-*.log。
