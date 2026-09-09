# LevelDB db_bench comparison of fixed commits

Throughput in Kops/s over valid samples only, relative column against `bpfbase`.

## fillrandom / mcs_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| bpfbase | `fbc463e` | 3 | 0 | 267.619 | 3.867 | 1.45 | 1.000x | 271.000 / 268.455 / 263.403 |
| bpftip | `432bee6` | 3 | 0 | 275.283 | 4.878 | 1.77 | 1.029x | 272.827 / 272.121 / 280.900 |

## fillrandom / mcs_tas_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| bpfbase | `fbc463e` | 3 | 0 | 265.029 | 11.849 | 4.47 | 1.000x | 272.498 / 271.223 / 251.367 |
| bpftip | `432bee6` | 3 | 0 | 276.653 | 8.098 | 2.93 | 1.044x | 271.869 / 272.087 / 286.002 |

## readrandom / mcs_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| bpfbase | `fbc463e` | 3 | 0 | 1338.797 | 9.970 | 0.74 | 1.000x | 1329.530 / 1337.515 / 1349.346 |
| bpftip | `432bee6` | 3 | 0 | 1211.525 | 6.459 | 0.53 | 0.905x | 1217.800 / 1211.878 / 1204.897 |

## readrandom / mcs_tas_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| bpfbase | `fbc463e` | 3 | 0 | 1162.158 | 21.830 | 1.88 | 1.000x | 1142.346 / 1185.560 / 1158.568 |
| bpftip | `432bee6` | 3 | 0 | 1101.615 | 14.320 | 1.30 | 0.948x | 1099.837 / 1116.741 / 1088.267 |

## Invalid runs

None.

