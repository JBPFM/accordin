---
active: true
iteration: 2
session_id: 
max_iterations: 10
completion_promise: null
started_at: "2026-04-01T12:48:12Z"
---

bench/mutexbench/scripts/sweep_mutex_throughput_multi_lock.sh --locks lb_simple --sudo-mode auto --threads 64 --critical-ns 350 --outside-ns 350 --duration-ms 3000 --warmup-duration-ms 1000 --repeats 3 --output-root results-tmp --timeslice-extension require 运行该命令，调整直到cpu pct和handoff time都达到最优。
