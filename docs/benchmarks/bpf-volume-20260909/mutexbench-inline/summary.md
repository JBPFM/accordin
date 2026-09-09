# mutexbench comparison of fixed commits

Throughput in Kops/s over valid samples only, relative column against `bpfbase`.

| case | arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---|---:|---:|---:|---:|---:|---:|---|
| low-t2-c100-o0 | bpfbase | `fbc463e` | 3 | 0 | 3543.9 | 59.7 | 1.69 | 1.000x | 3566.5 / 3589.0 / 3476.1 |
| low-t2-c100-o0 | inline | `0082ed7` | 3 | 0 | 3521.0 | 50.7 | 1.44 | 0.994x | 3492.9 / 3579.6 / 3490.7 |
| low-t4-c100-o0 | bpfbase | `fbc463e` | 3 | 0 | 3992.1 | 24.1 | 0.60 | 1.000x | 3976.5 / 3980.0 / 4019.8 |
| low-t4-c100-o0 | inline | `0082ed7` | 3 | 0 | 4010.8 | 15.5 | 0.39 | 1.005x | 4023.0 / 4016.1 / 3993.3 |
| low-t4-c100-o3000 | bpfbase | `fbc463e` | 3 | 0 | 1067.7 | 6.9 | 0.65 | 1.000x | 1075.1 / 1061.3 / 1066.5 |
| low-t4-c100-o3000 | inline | `0082ed7` | 3 | 0 | 1070.1 | 2.4 | 0.23 | 1.002x | 1069.0 / 1072.9 / 1068.4 |
| overload-t96-c100-o3000 | bpfbase | `fbc463e` | 3 | 0 | 2596.5 | 9.7 | 0.38 | 1.000x | 2585.3 / 2602.8 / 2601.5 |
| overload-t96-c100-o3000 | inline | `0082ed7` | 3 | 0 | 2580.0 | 40.3 | 1.56 | 0.994x | 2533.5 / 2601.1 / 2605.4 |

## Invalid runs

None.

