# LevelDB db_bench comparison of fixed commits

Throughput in Kops/s over valid samples only, relative column against `bpfbase`.

## fillrandom / mcs_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| bpfbase | `fbc463e` | 3 | 0 | 250.814 | 18.832 | 7.51 | 1.000x | 239.123 / 272.538 / 240.782 |
| inline | `0082ed7` | 3 | 0 | 246.074 | 11.390 | 4.63 | 0.981x | 243.475 / 236.209 / 258.539 |

## fillrandom / mcs_tas_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| bpfbase | `fbc463e` | 3 | 0 | 257.825 | 10.347 | 4.01 | 1.000x | 246.059 / 265.504 / 261.914 |
| inline | `0082ed7` | 3 | 0 | 255.895 | 15.404 | 6.02 | 0.993x | 239.686 / 270.342 / 257.656 |

## Invalid runs

None.

