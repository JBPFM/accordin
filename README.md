# Accordin Admission Direct

Accordin 将用户态锁与 Linux eBPF `sched_ext` admission 调度器结合。核心由 C11 锁运行时和 scx C/libbpf 加载器组成，仅保留两个显式 C API 后端：

| 动态库 | 接口 |
| --- | --- |
| `libmcs_accordin_direct.so` | MCS mutex；保留 `mcs_tas_accordin_direct_mutex_*` 兼容别名 |
| `libmcs_tas_accordin_direct.so` | MCS-TAS mutex |

应用通过链接或 `dlopen` 调用 direct API。动态库加载时初始化调度器；它们不导出 `pthread_mutex_*` 或 `pthread_cond_*`，仅设置 `LD_PRELOAD` 无法替换普通程序的 pthread 锁。

## 构建与验证

```sh
make -j
make check
sudo make check-bpf
# 将原始锁函数保留为独立的 perf 符号：
make OUT=target/perf PERF_SYMBOLS=1
# 至少允许两个 CPU 时，额外测试等待期间更改 affinity：
sudo env DIRECT_SMOKE_MIGRATE=1 bash scripts/test_direct_api.sh --bpf
```

构建需要 Clang（包含 BPF target）、bpftool、pkg-config 和 libbpf 开发包（建议 libbpf 1.5+）。例如 Ubuntu 上安装 `clang bpftool libbpf-dev libelf-dev zlib1g-dev pkg-config make`。无需 Rust 或 Cargo。两个库仍输出到 `target/release/`，也可用 `make mcs_accordin_direct` 或 `make mcs_tas_accordin_direct` 单独构建。

scx C 头文件固定在 `third_party/scx/`。构建时从 `/sys/kernel/btf/vmlinux` 生成 `vmlinux.h`，可用 `VMLINUX_BTF=/path/to/vmlinux.btf` 指定其他 BTF。实际调度需要支持 `sched_ext` 的 Linux 内核和 BPF 权限。`check-bpf` 会短时启动调度器，仅在当前没有其他 sched_ext 调度器时运行；可在命令前加 `taskset -c <CPU列表>` 验证受限 affinity。`bash gen-compile-commands.sh` 根据 Makefile 生成 clangd 编译数据库。

应用应在退出所有使用 direct API 的线程后再卸载动态库。MCS 后端保持每线程最多同时持有 4 把锁；调度器支持最多 256 个逻辑 CPU。

C 头文件位于 `include/mcs_accordin_direct.h` 和 `include/mcs_tas_accordin_direct.h`。例如：

```c
#include "mcs_tas_accordin_direct.h"

int main(void) {
    mcs_tas_accordin_direct_mutex_t *mutex =
        mcs_tas_accordin_direct_mutex_create();
    mcs_tas_accordin_direct_mutex_lock(mutex);
    /* critical section */
    mcs_tas_accordin_direct_mutex_unlock(mutex);
    return mcs_tas_accordin_direct_mutex_destroy(mutex);
}
```

```sh
cc example.c -Iinclude -Ltarget/release -lmcs_tas_accordin_direct \
  -Wl,-rpath,"$PWD/target/release" -o example
sudo ./example
# 无 BPF 的锁正确性检查：
MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF=1 ./example
```

## Admission 行为

核心规则：**每个逻辑 CPU 只为一个线程保留锁等待名额**。锁本身不再分配 ID，也没有锁数量上限。

1. 线程首次使用 direct mutex 时注册自己的 admission word。无竞争时可以直接获取锁。
2. 外层加锁进入慢路径时，发布 WAITING 并 yield。若线程仍拥有当前 CPU 的名额，BPF 用 CAS 将旧 ticket 更新为本次请求；否则进入全局 FIFO 等待队列。
3. CPU 有空闲名额时，从队列中取一个 affinity 允许的线程，先保留名额；用户态确认名额属于本次请求后，才进入原始锁的自旋队列。
4. 每次外层加锁使用递增的请求编号，用户态始终校验本次 ticket。解锁清除用户态状态，调度器观察到释放或未续用的新请求时回收名额，无需额外解锁 syscall；若下一次慢路径赶在回收前进入 yield，可以更新并续用自己的名额。

等待队列按 FIFO 扫描；已有名额的线程可以连续续用，因此不保证严格 FIFO 或有界等待。

普通线程与已获准线程都能继续得到调度，因此被抢占的持锁线程可以恢复并解锁。嵌套锁共享一次 admission，最后一把锁释放才结束；持有外层锁的线程不会因获取内层锁再次被 admission 阻塞。若已进入原始锁队列的线程更改 affinity，则允许它完成当前操作，避免将 MCS 前驱停在后继后面。这两类继续执行的路径不属于新等待者的准入限制。

实现只需要一个普通队列、一个新等待者队列，以及每 CPU 一个 owner 记录。已获准线程重新入队时，直接进入名额所属 CPU 的 local 队列。已删除按锁分类、随机选队列、概率参数、扫描预算、序列门控和定时强制放行。

核心不再包含条件变量（CV）、writer-event、width control、锁类合并、动态 CPU affinity、CS/NCS/wait 采样或 timeslice extension。CPU 范围继承应用的 affinity，可通过外部 `taskset` 设置。

## 运行开关

下表中的 `<PREFIX>` 为 `MCS_ACCORDIN_DIRECT` 或 `MCS_TAS_ACCORDIN_DIRECT`。

| 环境变量 | 行为与默认值 |
| --- | --- |
| `<PREFIX>_DISABLE_BPF` | 默认关闭；设为 `1` 时不加载 BPF。 |
| `ACCORDIN_DISABLE_ADMISSION` | 默认关闭；设为 `1` 时不发布 admission 标志。 |
| `<PREFIX>_STATS_ONLY` | 保留历史名称的对照模式；设为 `1` 时加载普通 sched_ext 调度，不进行锁感知路由，也不采样锁时间。 |

旧 CV、width、固定 width、依赖图、动态 CPU 和采样环境变量已不再解析。CV/writer-event 和动态 CPU 控制的 C 符号及旧时间统计输出已移除。旧 `DEBUG_COUNTERS`、`INACTIVE_PREVIOUS_LOCK_PERCENT` 参数和 `[lock_stats]` 计数输出也已移除。调度异常时，BPF dump 保留队列长度和 CPU owner 信息。

## 实验与历史入口

192 线程下的 Rust → C 迁移性能对比、原始日志和复现命令见 [测量报告](docs/benchmarks/c-migration-192/README.md)。

当前 local 队列与授权更新的合入验证见 [验证记录](docs/benchmarks/local-owner-regression/integration.md)。

实验、benchmark 子模块、第三方源码、结果和历史设计文档保留。mutexbench 的 direct 实验 1、3、6–9 继续使用原库名和 mutex C ABI。实验 4 的 LevelDB direct 选项依赖已移除的 CV/writer-event API，因此已停用；普通 mutex 等外部基线选项和历史结果绘图仍可使用。`patches/leveldb-accordin-direct.patch` 仅保留作历史参考。

依赖已删除的 `accordin`、`mcs_accordin`、`mcs_tas_accordin`、`ttas_accordin`、`reciprocating_accordin` 或 `mcs_tse` preload 的历史入口已停用，会在使用这些库前报错，即使构建目录中仍有旧库。`run_benchmark.sh` 也已停用。外部 benchmark 子模块中的旧说明和脚本保留作历史参考；不属于当前核心支持的运行入口。

实验标签可能仍叫 `mcs_accordin` 或 `mcs_tas_accordin_admission_only`；在支持 direct 的实验中，它们映射到对应 direct 库。历史文档描述的是当时版本，不代表当前功能。
