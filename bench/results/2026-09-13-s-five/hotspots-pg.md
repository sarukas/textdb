# Optimisation candidates — textdb-pg

Source: `bench/out-five/results.jsonl` · baselines: fs, sql-text-sqlite, sql-text-pg

## By operation

Time is `p50 x calls`, summed over the tests that used the operation — an
estimate of where the run's time went, not a measured total.

| operation | calls | textdb-pg p50 | est. time | best baseline | ratio |
|---|---|---|---|---|---|
| replace | 10222 | 31.31 ms | 320.01 s | fs 380 us | 82.29x |
| read | 69991 | 2.59 ms | 181.03 s | sql-text-sqlite 13 us | 205.80x |
| read_version | 4396 | 12.16 ms | 53.47 s | sql-text-sqlite 1.14 ms | 10.66x |
| search | 744 | 47.62 ms | 35.43 s | sql-text-sqlite 1.60 ms | 29.79x |
| create | 4848 | 5.98 ms | 29.01 s | fs 178 us | 33.53x |
| append | 1275 | 18.02 ms | 22.98 s | fs 169 us | 106.40x |
| read_lines | 24051 | 2.27 ms | 54.71 s | fs 1.52 ms | 1.50x |
| maintenance | 1 | 228.06 ms | 228.06 ms | sql-text-sqlite 79.02 ms | 2.89x |
| rename | 1 | 16.36 ms | 16.36 ms | fs 13 us | 1305.89x |
| history | 9 | 824 us | 7.41 ms | sql-text-sqlite 143 us | 5.74x |
| list | 2 | 1.41 ms | 2.81 ms | sql-text-sqlite 510 us | 2.76x |
| delete | 1 | 6.28 ms | 6.28 ms | fs 4.79 ms | 1.31x |

## Slowest cells relative to baseline

Per test/case p99, where the target is slower than the best baseline. Ranked by
the gap, so the top rows are where a fix changes the most.

| test | case | metric | textdb-pg | best baseline | ratio |
|---|---|---|---|---|---|
| CW-02 | N=50 | write | 51.06 s | fs 37.48 ms | 1362.33x |
| CW-01 | N=50 | write | 46.50 s | fs 37.85 ms | 1228.67x |
| LL-01..03 | 32MiB | create | 9.45 s | fs 174.94 ms | 54.02x |
| XL-01..06 | 10MiB | create | 2.76 s | fs 54.30 ms | 50.90x |
| CW-02 | N=20 | write | 1.32 s | fs 19.53 ms | 67.84x |
| CW-04 | N=50 | write | 1.23 s | sql-text-pg 18.45 ms | 66.44x |
| CW-01 | N=20 | write | 1.13 s | fs 9.56 ms | 118.74x |
| LL-01..03 | 32MiB | read | 913.16 ms | fs 6.00 ms | 152.07x |
| LL-01..03 | 32MiB | read_lines_1_1 | 860.78 ms | fs 51.64 ms | 16.67x |
| CW-05 | N=50 | write | 678.87 ms | fs 26.16 ms | 25.95x |
| CR-06 | N=50 | read_version | 613.26 ms | sql-text-sqlite 4.84 ms | 126.62x |
| CR-03 | N=100 | writer_replace | 525.85 ms | fs 8.91 ms | 59.01x |
| RT-03 | 1MiB/lf/mixed | create | 342.65 ms | fs 343 us | 1000.27x |
| RT-03 | all | create | 342.65 ms | fs 456 us | 751.12x |
| LL-01..03 | 1MiB | create | 293.47 ms | fs 420 us | 699.51x |
| RT-03 | 1MiB/lf/ascii | create | 292.38 ms | fs 456 us | 640.92x |
| RT-01 | 1MiB/lf/mixed | create | 275.03 ms | fs 355 us | 775.01x |
| RT-01 | all | create | 275.03 ms | fs 362 us | 759.46x |
| XL-01..06 | 10MiB | read | 231.18 ms | sql-text-sqlite 4.90 ms | 47.13x |
| CR-05 | N=20 | search | 238.71 ms | sql-text-sqlite 26.78 ms | 8.91x |
| XL-01..06 | 10MiB | read_version_v1 | 232.14 ms | sql-text-sqlite 31.95 ms | 7.27x |
| XL-01..06 | 10MiB | seq_replace | 230.24 ms | fs 44.44 ms | 5.18x |
| XL-01..06 | 10MiB@100% | replace | 218.71 ms | fs 42.97 ms | 5.09x |
| CW-05 | N=20 | write | 157.15 ms | fs 6.74 ms | 23.30x |
| XL-01..06 | 10MiB@50% | replace | 215.56 ms | fs 85.05 ms | 2.53x |

## Footprint

| test | case | metric | textdb-pg | fs | sql-text-sqlite | sql-text-pg |
|---|---|---|---|---|---|---|
| FP-01/02 | – | footprint_after_maintenance | 11,427,840 B | – | 16,826,304 B | 7,282,688 B |
| FP-01/02 | – | footprint_after_maintenance_over_raw | 7.95x | – | 11.71x | 5.07x |
| FP-01/02 | after_0 | footprint_bytes | 11,304,960 B | 1,474,766 B | 10,102,256 B | 10,133,504 B |
| FP-01/02 | after_0 | footprint_over_raw | 7.67x | 1.00x | 6.85x | 6.87x |
| FP-01/02 | after_1200 | footprint_bytes | 15,155,200 B | 1,437,405 B | 12,989,936 B | 15,171,584 B |
| FP-01/02 | after_1200 | footprint_over_raw | 10.54x | 1.00x | 9.04x | 10.55x |
| FP-01/02 | after_300 | footprint_bytes | 12,337,152 B | 1,463,905 B | 11,232,752 B | 11,649,024 B |
| FP-01/02 | after_300 | footprint_over_raw | 8.43x | 1.00x | 7.67x | 7.96x |
| FP-01/02 | after_600 | footprint_bytes | 13,328,384 B | 1,454,040 B | 11,757,040 B | 12,836,864 B |
| FP-01/02 | after_600 | footprint_over_raw | 9.17x | 1.00x | 8.09x | 8.83x |
| FP-01/02 | after_900 | footprint_bytes | 14,245,888 B | 1,444,903 B | 12,658,160 B | 14,049,280 B |
| FP-01/02 | after_900 | footprint_over_raw | 9.86x | 1.00x | 8.76x | 9.72x |
| LL-04 | – | footprint_bytes | 18,538,496 B | 3,145,361 B | 84,385,000 B | – |
| LL-04 | – | footprint_growth_bytes | 229,376 B | 0 B | 67,476,224 B | – |
| LL-04 | – | footprint_over_raw | 5.89x | 1.00x | 26.83x | – |
| LL-05 | – | footprint_bytes | 16,171,008 B | 2,980,041 B | 82,992,040 B | 57,966,592 B |
| LL-05 | – | footprint_growth_bytes | 270,336 B | 0 B | 68,244,072 B | 54,960,128 B |
| LL-05 | – | footprint_over_raw | 5.43x | 1.00x | 27.85x | 19.45x |
| ME-01/02 | – | footprint_bytes | 5,169,152 B | 30,716 B | 23,782,128 B | 40,263,680 B |
| ME-01/02 | – | footprint_growth_bytes | 4,571,136 B | 0 B | 23,518,632 B | 40,009,728 B |
| ME-01/02 | – | footprint_over_raw | 168.29x | 1.00x | 774.26x | 1310.84x |
| ME-01/02 | after_150 | footprint_bytes | 1,810,432 B | 30,714 B | 8,544,336 B | 12,681,216 B |
| ME-01/02 | after_300 | footprint_bytes | 2,949,120 B | 30,717 B | 13,701,848 B | 22,396,928 B |
| ME-01/02 | after_450 | footprint_bytes | 4,046,848 B | 30,713 B | 18,723,544 B | 31,113,216 B |
| ME-01/02 | after_600 | footprint_bytes | 5,169,152 B | 30,716 B | 23,782,128 B | 40,263,680 B |
| ME-03 | – | footprint_bytes | 3,694,592 B | 41,410 B | 26,358,008 B | 34,512,896 B |
| ME-03 | – | footprint_growth_bytes | 3,096,576 B | 10,690 B | 26,094,512 B | 34,258,944 B |
| ME-03 | – | footprint_over_raw | 89.22x | 1.00x | 636.51x | 833.44x |
| ME-04 | – | footprint_bytes | 1,179,648 B | 30,649 B | 8,151,000 B | 12,681,216 B |
| ME-04 | – | footprint_growth_bytes | 581,632 B | 0 B | 7,887,504 B | 12,427,264 B |
| ME-04 | – | footprint_over_raw | 38.49x | 1.00x | 265.95x | 413.76x |
| ME-05 | – | footprint_bytes | 20,504,576 B | 296,516 B | 186,700,152 B | 217,169,920 B |
| ME-05 | – | footprint_growth_bytes | 18,030,592 B | 0 B | 185,781,576 B | 216,580,096 B |
| ME-05 | – | footprint_over_raw | 69.15x | 1.00x | 629.65x | 732.41x |
| RT-04 | – | footprint_bytes | 1,318,912 B | 26,667 B | 5,389,912 B | 4,816,896 B |
| RT-04 | – | footprint_growth_bytes | 720,896 B | 0 B | 5,126,416 B | 4,562,944 B |
| RT-04 | – | footprint_over_raw | 49.46x | 1.00x | 202.12x | 180.63x |
| RT-05 | – | footprint_bytes | 1,425,408 B | 30,151 B | 5,332,448 B | 5,447,680 B |
| RT-05 | – | footprint_growth_bytes | 827,392 B | 0 B | 5,068,952 B | 5,193,728 B |
| RT-05 | – | footprint_over_raw | 47.28x | 1.00x | 176.86x | 180.68x |
| RT-06 | – | footprint_after_import | 3,309,568 B | 368,682 B | 5,111,648 B | 3,080,192 B |
| RT-06 | – | footprint_over_raw | 8.98x | 1.00x | 13.86x | 8.35x |
| SR-01/02 | – | footprint_after_import | 8,724,480 B | 1,106,055 B | 7,368,544 B | 8,863,744 B |
| SR-01/02 | – | footprint_over_raw | 7.89x | 1.00x | 6.66x | 8.01x |
| SR-04 | – | footprint_after_import | 6,045,696 B | 737,368 B | 6,172,512 B | 5,980,160 B |
| SR-04 | – | footprint_after_maintenance | 6,160,384 B | – | 7,759,656 B | 4,423,680 B |
| SR-04 | – | footprint_growth_after_edits | 3,145,728 B | 0 B | 1,587,144 B | 3,481,600 B |
| SR-04 | – | footprint_growth_per_edit | 7,149 B | 0 B | 3,607 B | 7,913 B |
| SR-04 | – | footprint_over_raw | 8.20x | 1.00x | 8.37x | 8.11x |
| SR-05 | – | footprint_after_import | 8,724,480 B | 1,106,055 B | 7,368,544 B | 8,863,744 B |
| SR-05 | – | footprint_over_raw | 7.89x | 1.00x | 6.66x | 8.01x |
| XL-01..06 | 10MiB | footprint_after_create | 42,344,448 B | 10,485,760 B | 48,540,032 B | 10,985,472 B |
| XL-01..06 | 10MiB | footprint_growth_per_edit | 11,469 B | 0 B | 10,495,590 B | 11,116,544 B |
| XL-01..06 | 10MiB@100% | replace_footprint_growth | 16,384 B | 0 B | 14,099,464 B | 10,911,744 B |
| XL-01..06 | 10MiB@50% | replace_footprint_growth | 8,192 B | 0 B | 10,493,952 B | 10,903,552 B |

> 1 cell(s) published no timings because an accuracy check failed:
> - LL-04 / sql-text-pg
