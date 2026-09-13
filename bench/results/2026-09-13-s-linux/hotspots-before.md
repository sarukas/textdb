# Optimisation candidates — textdb-sqlite

Source: `/tmp/claude-0/-home-user-textdb/5814c509-fe09-51c7-b73d-9cb51af0f9f1/scratchpad/before-sqltext.jsonl` · baselines: sql-text-sqlite

## By operation

Time is `p50 x calls`, summed over the tests that used the operation — an
estimate of where the run's time went, not a measured total.

| operation | calls | textdb-sqlite p50 | est. time | best baseline | ratio |
|---|---|---|---|---|---|
| read_version | 19075 | 4.94 ms | 94.17 s | sql-text-sqlite 1.68 ms | 2.95x |
| search | 1852 | 19.89 ms | 36.84 s | sql-text-sqlite 1.77 ms | 11.24x |
| replace | 10512 | 2.71 ms | 28.46 s | sql-text-sqlite 723 us | 3.74x |
| read | 2167263 | 16 us | 35.49 s | sql-text-sqlite 15 us | 1.12x |
| create | 4849 | 1.11 ms | 5.40 s | sql-text-sqlite 649 us | 1.71x |
| list | 2 | 3.68 ms | 7.36 ms | sql-text-sqlite 545 us | 6.76x |
| history | 9 | 379 us | 3.42 ms | sql-text-sqlite 140 us | 2.71x |
| rename | 1 | 2.54 ms | 2.54 ms | sql-text-sqlite 20.10 ms | 0.13x |
| read_lines | 59640 | 619 us | 36.93 s | sql-text-sqlite 7.53 ms | 0.08x |
| maintenance | 1 | 58.10 ms | 58.10 ms | sql-text-sqlite 79.87 ms | 0.73x |
| delete | 1 | 1.46 ms | 1.46 ms | sql-text-sqlite 33.84 ms | 0.04x |
| append | 1275 | 732 us | 933.58 ms | sql-text-sqlite 1.48 ms | 0.49x |

## Slowest cells relative to baseline

Per test/case p99, where the target is slower than the best baseline. Ranked by
the gap, so the top rows are where a fix changes the most.

| test | case | metric | textdb-sqlite | best baseline | ratio |
|---|---|---|---|---|---|
| CW-04 | N=50 | write | 1.04 s | sql-text-sqlite 333.11 ms | 3.11x |
| CW-02 | N=50 | write | 1.13 s | sql-text-sqlite 532.93 ms | 2.13x |
| LL-01..03 | 32MiB | create | 1.51 s | sql-text-sqlite 958.36 ms | 1.58x |
| CW-01 | N=50 | write | 1.16 s | sql-text-sqlite 733.12 ms | 1.58x |
| CW-07 | N=20 | write | 333.09 ms | sql-text-sqlite 78.95 ms | 4.22x |
| LL-01..03 | 32MiB | read | 228.36 ms | sql-text-sqlite 42.76 ms | 5.34x |
| XL-01..06 | 10MiB | create | 466.67 ms | sql-text-sqlite 334.99 ms | 1.39x |
| CW-01 | N=20 | write | 436.18 ms | sql-text-sqlite 331.04 ms | 1.32x |
| CW-04 | N=20 | write | 183.07 ms | sql-text-sqlite 130.17 ms | 1.41x |
| CW-06 | N=20 | write | 180.32 ms | sql-text-sqlite 129.90 ms | 1.39x |
| CR-05 | N=20 | search | 63.02 ms | sql-text-sqlite 25.38 ms | 2.48x |
| NS-02 | – | create | 32.44 ms | sql-text-sqlite 182 us | 178.27x |
| RT-01 | all | create | 49.62 ms | sql-text-sqlite 20.65 ms | 2.40x |
| RT-01 | 1MiB/lf/mixed | create | 49.62 ms | sql-text-sqlite 20.65 ms | 2.40x |
| RT-03 | 1MiB/lf/ascii | create | 42.79 ms | sql-text-sqlite 16.57 ms | 2.58x |
| RT-03 | all | create | 120.35 ms | sql-text-sqlite 94.78 ms | 1.27x |
| RT-03 | 1MiB/lf/random_bytes | create | 120.35 ms | sql-text-sqlite 94.78 ms | 1.27x |
| CW-01 | N=5 | write | 55.66 ms | sql-text-sqlite 33.89 ms | 1.64x |
| RT-03 | 1MiB/lf/mixed | create | 64.35 ms | sql-text-sqlite 46.16 ms | 1.39x |
| XL-01..06 | 10MiB | read | 21.22 ms | sql-text-sqlite 4.96 ms | 4.28x |
| LL-01..03 | 1MiB | create | 27.43 ms | sql-text-sqlite 15.88 ms | 1.73x |
| LL-06 | 8MiB | read | 12.46 ms | sql-text-sqlite 4.15 ms | 3.00x |
| CR-03 | N=100 | read | 10.88 ms | sql-text-sqlite 4.34 ms | 2.51x |
| CR-06 | N=50 | read_version | 12.19 ms | sql-text-sqlite 5.94 ms | 2.05x |
| CR-01 | N=100 | read | 10.41 ms | sql-text-sqlite 4.92 ms | 2.12x |

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
