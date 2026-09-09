# mutexbench comparison of fixed commits

Throughput in Kops/s over valid samples only, relative column against `bpfbase`.

| case | arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---|---:|---:|---:|---:|---:|---:|---|
| low-t2-c100-o0 | bpfbase | `fbc463e` | 3 | 0 | 3497.5 | 119.7 | 3.42 | 1.000x | 3359.3 / 3563.4 / 3569.9 |
| low-t2-c100-o0 | bpftip | `432bee6` | 3 | 0 | 3416.9 | 129.5 | 3.79 | 0.977x | 3410.2 / 3290.9 / 3549.5 |
| low-t4-c100-o0 | bpfbase | `fbc463e` | 3 | 0 | 4008.2 | 12.9 | 0.32 | 1.000x | 4011.5 / 3994.0 / 4019.1 |
| low-t4-c100-o0 | bpftip | `432bee6` | 3 | 0 | 4054.5 | 41.9 | 1.03 | 1.012x | 4017.0 / 4046.9 / 4099.7 |
| low-t4-c100-o3000 | bpfbase | `fbc463e` | 3 | 0 | 1072.8 | 2.2 | 0.21 | 1.000x | 1075.2 / 1072.4 / 1070.8 |
| low-t4-c100-o3000 | bpftip | `432bee6` | 3 | 0 | 1072.4 | 1.3 | 0.12 | 1.000x | 1071.7 / 1071.7 / 1073.9 |
| overload-t96-c100-o3000 | bpfbase | `fbc463e` | 3 | 0 | 2597.6 | 9.9 | 0.38 | 1.000x | 2604.5 / 2602.0 / 2586.3 |
| overload-t96-c100-o3000 | bpftip | `432bee6` | 3 | 0 | 2617.0 | 8.1 | 0.31 | 1.007x | 2607.7 / 2622.4 / 2621.0 |

## Invalid runs

None.

