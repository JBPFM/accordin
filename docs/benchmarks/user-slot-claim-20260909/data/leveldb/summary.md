# LevelDB db_bench comparison of fixed commits

Throughput in Kops/s over valid samples only, relative column against `yield`.

## readrandom / mcs_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| yield | `9fc1b0a` | 3 | 0 | 1195.016 | 19.244 | 1.61 | 1.000x | 1195.867 / 1213.821 / 1175.360 |
| claim | `9fc1b0a` | 3 | 0 | 1180.800 | 29.076 | 2.46 | 0.988x | 1148.181 / 1190.222 / 1203.996 |

## readrandom / mcs_tas_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| yield | `9fc1b0a` | 3 | 0 | 1101.587 | 4.960 | 0.45 | 1.000x | 1096.155 / 1102.730 / 1105.876 |
| claim | `9fc1b0a` | 3 | 0 | 1065.457 | 34.969 | 3.28 | 0.967x | 1025.777 / 1078.819 / 1091.776 |

## fillrandom / mcs_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| yield | `9fc1b0a` | 3 | 0 | 263.180 | 3.307 | 1.26 | 1.000x | 266.619 / 260.023 / 262.899 |
| claim | `9fc1b0a` | 3 | 0 | 256.273 | 9.840 | 3.84 | 0.974x | 267.603 / 249.860 / 251.356 |

## fillrandom / mcs_tas_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| yield | `9fc1b0a` | 3 | 0 | 261.554 | 12.743 | 4.87 | 1.000x | 264.484 / 247.602 / 272.578 |
| claim | `9fc1b0a` | 3 | 0 | 258.964 | 12.425 | 4.80 | 0.990x | 267.337 / 244.688 / 264.866 |

## Invalid runs

None.

