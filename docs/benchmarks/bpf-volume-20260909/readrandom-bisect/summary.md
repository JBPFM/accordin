# LevelDB db_bench comparison of fixed commits

Throughput in Kops/s over valid samples only, relative column against `bpfbase`.

## readrandom / mcs_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| bpfbase | `fbc463e` | 3 | 0 | 1350.270 | 17.990 | 1.33 | 1.000x | 1369.627 / 1347.120 / 1334.062 |
| subprog | `be0dd22` | 3 | 0 | 1161.810 | 22.313 | 1.92 | 0.860x | 1142.376 / 1186.177 / 1156.876 |
| flags | `2a872ce` | 3 | 0 | 1340.455 | 1.307 | 0.10 | 0.993x | 1341.935 / 1339.459 / 1339.970 |
| bpftip | `432bee6` | 3 | 0 | 1187.407 | 18.443 | 1.55 | 0.879x | 1166.620 / 1201.811 / 1193.791 |
| inline | `0082ed7` | 3 | 0 | 1344.611 | 8.505 | 0.63 | 0.996x | 1346.007 / 1335.494 / 1352.331 |

## readrandom / mcs_tas_accordin

| arm | commit | n valid | n invalid | mean Kops/s | stdev | CV% | relative | runs |
|---|---|---:|---:|---:|---:|---:|---:|---|
| bpfbase | `fbc463e` | 3 | 0 | 1174.194 | 17.349 | 1.48 | 1.000x | 1192.520 / 1158.023 / 1172.039 |
| subprog | `be0dd22` | 3 | 0 | 1072.501 | 23.657 | 2.21 | 0.913x | 1047.799 / 1094.951 / 1074.754 |
| flags | `2a872ce` | 3 | 0 | 1170.681 | 21.788 | 1.86 | 0.997x | 1167.702 / 1150.535 / 1193.805 |
| bpftip | `432bee6` | 3 | 0 | 1064.044 | 9.740 | 0.92 | 0.906x | 1054.577 / 1063.521 / 1074.035 |
| inline | `0082ed7` | 3 | 0 | 1171.678 | 27.034 | 2.31 | 0.998x | 1189.480 / 1184.986 / 1140.570 |

## Invalid runs

None.

