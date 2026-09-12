# Optimisation candidates — textdb-sqlite

Source: `bench/out-final/results.jsonl` · baselines: sql-text-sqlite

## By operation

Time is `p50 x calls`, summed over the tests that used the operation — an
estimate of where the run's time went, not a measured total.

| operation | calls | textdb-sqlite p50 | est. time | best baseline | ratio |
|---|---|---|---|---|---|
| read | 1059078 | 75 us | 79.70 s | sql-text-sqlite 46 us | 1.62x |
| search | 1547 | 20.07 ms | 31.05 s | sql-text-sqlite 2.35 ms | 8.54x |
| read_version | 16427 | 1.31 ms | 21.56 s | sql-text-sqlite 664 us | 1.98x |
| create | 4849 | 2.10 ms | 10.17 s | sql-text-sqlite 1.33 ms | 1.58x |
| list | 2 | 7.67 ms | 15.34 ms | sql-text-sqlite 609 us | 12.58x |
| history | 9 | 574 us | 5.17 ms | sql-text-sqlite 206 us | 2.78x |
| replace | 8581 | 2.12 ms | 18.20 s | sql-text-sqlite 2.48 ms | 0.86x |
| rename | 1 | 8.91 ms | 8.91 ms | sql-text-sqlite 39.84 ms | 0.22x |
| read_lines | 43151 | 790 us | 34.09 s | sql-text-sqlite 158.76 ms | 0.00x |
| maintenance | 1 | 75.12 ms | 75.12 ms | sql-text-sqlite 116.60 ms | 0.64x |
| delete | 1 | 2.64 ms | 2.64 ms | sql-text-sqlite 55.81 ms | 0.05x |
| append | 1275 | 1.29 ms | 1.64 s | sql-text-sqlite 2.68 ms | 0.48x |

## Slowest cells relative to baseline

Per test/case p99, where the target is slower than the best baseline. Ranked by
the gap, so the top rows are where a fix changes the most.

| test | case | metric | textdb-sqlite | best baseline | ratio |
|---|---|---|---|---|---|
| LL-01..03 | 32MiB | create | 2.69 s | sql-text-sqlite 1.80 s | 1.49x |
| CW-06 | N=20 | write | 1.02 s | sql-text-sqlite 269.67 ms | 3.78x |
| CW-07 | N=20 | write | 871.10 ms | sql-text-sqlite 190.99 ms | 4.56x |
| CW-04 | N=50 | write | 2.30 s | sql-text-sqlite 1.64 s | 1.40x |
| CW-02 | N=20 | write | 1.41 s | sql-text-sqlite 887.88 ms | 1.59x |
| CW-03 | N=20 | write | 1.32 s | sql-text-sqlite 1.09 s | 1.21x |
| ME-06 | N=20 | write | 1.42 s | sql-text-sqlite 1.21 s | 1.17x |
| CR-06 | N=50 | read_version | 141.26 ms | sql-text-sqlite 32.42 ms | 4.36x |
| XL-01..06 | 10MiB | create | 743.78 ms | sql-text-sqlite 653.57 ms | 1.14x |
| CR-05 | N=20 | search | 117.05 ms | sql-text-sqlite 44.52 ms | 2.63x |
| RT-03 | all | create | 161.44 ms | sql-text-sqlite 111.09 ms | 1.45x |
| RT-03 | 1MiB/lf/random_bytes | create | 161.44 ms | sql-text-sqlite 111.09 ms | 1.45x |
| NS-02 | – | create | 49.05 ms | sql-text-sqlite 224 us | 218.48x |
| CW-04 | N=5 | write | 98.48 ms | sql-text-sqlite 62.15 ms | 1.58x |
| RT-01 | all | create | 66.02 ms | sql-text-sqlite 31.66 ms | 2.09x |
| RT-01 | 1MiB/lf/mixed | create | 66.02 ms | sql-text-sqlite 31.66 ms | 2.09x |
| RT-03 | 1MiB/lf/mixed | create | 96.94 ms | sql-text-sqlite 65.59 ms | 1.48x |
| RT-03 | 1MiB/lf/ascii | create | 56.46 ms | sql-text-sqlite 26.85 ms | 2.10x |
| LL-01..03 | 1MiB | create | 48.68 ms | sql-text-sqlite 23.12 ms | 2.11x |
| SR-01/02 | prefix | search | 27.02 ms | sql-text-sqlite 12.41 ms | 2.18x |
| NS-02 | – | list | 10.97 ms | sql-text-sqlite 31 us | 353.88x |
| XL-01..06 | 10MiB | read | 25.93 ms | sql-text-sqlite 16.45 ms | 1.58x |
| RT-02 | 100KiB/no_trailing/mixed | create | 11.27 ms | sql-text-sqlite 3.63 ms | 3.11x |
| LL-06 | 8MiB | read | 22.31 ms | sql-text-sqlite 14.80 ms | 1.51x |
| RT-02 | all | create | 11.27 ms | sql-text-sqlite 4.92 ms | 2.29x |

## Footprint

| test | case | metric | textdb-sqlite | sql-text-sqlite |
|---|---|---|---|---|
| FP-01/02 | – | footprint_after_maintenance | 12,095,096 B | 16,826,304 B |
| FP-01/02 | – | footprint_after_maintenance_over_raw | 8.41x | 11.71x |
| FP-01/02 | after_0 | footprint_bytes | 7,459,016 B | 10,102,256 B |
| FP-01/02 | after_0 | footprint_over_raw | 5.06x | 6.85x |
| FP-01/02 | after_1200 | footprint_bytes | 9,572,552 B | 12,989,936 B |
| FP-01/02 | after_1200 | footprint_over_raw | 6.66x | 9.04x |
| FP-01/02 | after_300 | footprint_bytes | 8,134,856 B | 11,232,752 B |
| FP-01/02 | after_300 | footprint_over_raw | 5.56x | 7.67x |
| FP-01/02 | after_600 | footprint_bytes | 8,487,112 B | 11,757,040 B |
| FP-01/02 | after_600 | footprint_over_raw | 5.84x | 8.09x |
| FP-01/02 | after_900 | footprint_bytes | 9,425,096 B | 12,658,160 B |
| FP-01/02 | after_900 | footprint_over_raw | 6.52x | 8.76x |
| LL-04 | – | footprint_bytes | 14,649,448 B | 84,385,000 B |
| LL-04 | – | footprint_growth_bytes | 0 B | 67,476,224 B |
| LL-04 | – | footprint_over_raw | 4.66x | 26.83x |
| LL-05 | – | footprint_bytes | 12,209,296 B | 82,992,040 B |
| LL-05 | – | footprint_growth_bytes | 0 B | 68,244,072 B |
| LL-05 | – | footprint_over_raw | 4.10x | 27.85x |
| ME-01/02 | – | footprint_bytes | 5,759,056 B | 23,782,128 B |
| ME-01/02 | – | footprint_growth_bytes | 5,404,920 B | 23,518,632 B |
| ME-01/02 | – | footprint_over_raw | 187.49x | 774.26x |
| ME-01/02 | after_150 | footprint_bytes | 4,636,344 B | 8,544,336 B |
| ME-01/02 | after_300 | footprint_bytes | 5,054,472 B | 13,701,848 B |
| ME-01/02 | after_450 | footprint_bytes | 5,390,344 B | 18,723,544 B |
| ME-01/02 | after_600 | footprint_bytes | 5,759,056 B | 23,782,128 B |
| ME-03 | – | footprint_bytes | 5,439,688 B | 26,358,008 B |
| ME-03 | – | footprint_growth_bytes | 5,085,552 B | 26,094,512 B |
| ME-03 | – | footprint_over_raw | 131.36x | 636.51x |
| ME-04 | – | footprint_bytes | 4,484,912 B | 8,151,000 B |
| ME-04 | – | footprint_growth_bytes | 4,130,776 B | 7,887,504 B |
| ME-04 | – | footprint_over_raw | 146.33x | 265.95x |
| ME-05 | – | footprint_bytes | 9,449,864 B | 186,700,152 B |
| ME-05 | – | footprint_growth_bytes | 8,543,648 B | 185,781,576 B |
| ME-05 | – | footprint_over_raw | 31.87x | 629.65x |
| RT-04 | – | footprint_bytes | 4,624,080 B | 5,389,912 B |
| RT-04 | – | footprint_growth_bytes | 4,269,944 B | 5,126,416 B |
| RT-04 | – | footprint_over_raw | 173.40x | 202.12x |
| RT-05 | – | footprint_bytes | 4,570,688 B | 5,332,448 B |
| RT-05 | – | footprint_growth_bytes | 4,216,552 B | 5,068,952 B |
| RT-05 | – | footprint_over_raw | 151.59x | 176.86x |
| RT-06 | – | footprint_after_import | 5,218,312 B | 5,111,648 B |
| RT-06 | – | footprint_over_raw | 14.15x | 13.86x |
| SR-01/02 | – | footprint_after_import | 6,770,768 B | 7,368,544 B |
| SR-01/02 | – | footprint_over_raw | 6.12x | 6.66x |
| SR-04 | – | footprint_after_import | 6,029,392 B | 6,172,512 B |
| SR-04 | – | footprint_after_maintenance | 7,504,048 B | 7,759,656 B |
| SR-04 | – | footprint_growth_after_edits | 1,474,656 B | 1,587,144 B |
| SR-04 | – | footprint_growth_per_edit | 3,351 B | 3,607 B |
| SR-04 | – | footprint_over_raw | 8.18x | 8.37x |
| SR-05 | – | footprint_after_import | 6,770,768 B | 7,368,544 B |
| SR-05 | – | footprint_over_raw | 6.12x | 6.66x |
| XL-01..06 | 10MiB | footprint_after_create | 40,225,344 B | 48,540,032 B |
| XL-01..06 | 10MiB | footprint_growth_per_edit | 0 B | 10,495,590 B |
| XL-01..06 | 10MiB@100% | replace_footprint_growth | 0 B | 14,099,464 B |
| XL-01..06 | 10MiB@50% | replace_footprint_growth | 0 B | 10,493,952 B |
