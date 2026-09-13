# Optimisation candidates — textdb-sqlite

Source: `/tmp/claude-0/-home-user-textdb/5814c509-fe09-51c7-b73d-9cb51af0f9f1/scratchpad/after-sqltext.jsonl` · baselines: sql-text-sqlite

## By operation

Time is `p50 x calls`, summed over the tests that used the operation — an
estimate of where the run's time went, not a measured total.

| operation | calls | textdb-sqlite p50 | est. time | best baseline | ratio |
|---|---|---|---|---|---|
| read_version | 16577 | 5.74 ms | 95.16 s | sql-text-sqlite 1.94 ms | 2.97x |
| search | 1881 | 19.50 ms | 36.67 s | sql-text-sqlite 1.71 ms | 11.41x |
| replace | 10494 | 2.52 ms | 26.47 s | sql-text-sqlite 809 us | 3.12x |
| create | 4849 | 984 us | 4.77 s | sql-text-sqlite 636 us | 1.55x |
| history | 9 | 424 us | 3.81 ms | sql-text-sqlite 150 us | 2.83x |
| list | 2 | 1.60 ms | 3.20 ms | sql-text-sqlite 478 us | 3.35x |
| rename | 1 | 3.43 ms | 3.43 ms | sql-text-sqlite 17.94 ms | 0.19x |
| read_lines | 52352 | 668 us | 34.99 s | sql-text-sqlite 9.85 ms | 0.07x |
| read | 4161080 | 9 us | 37.60 s | sql-text-sqlite 15 us | 0.61x |
| maintenance | 1 | 71.84 ms | 71.84 ms | sql-text-sqlite 86.78 ms | 0.83x |
| delete | 1 | 1.40 ms | 1.40 ms | sql-text-sqlite 34.22 ms | 0.04x |
| append | 1275 | 585 us | 746.00 ms | sql-text-sqlite 1.55 ms | 0.38x |

## Slowest cells relative to baseline

Per test/case p99, where the target is slower than the best baseline. Ranked by
the gap, so the top rows are where a fix changes the most.

| test | case | metric | textdb-sqlite | best baseline | ratio |
|---|---|---|---|---|---|
| CW-04 | N=50 | write | 1.53 s | sql-text-sqlite 229.78 ms | 6.68x |
| CW-01 | N=50 | write | 1.84 s | sql-text-sqlite 832.59 ms | 2.21x |
| LL-01..03 | 32MiB | create | 1.50 s | sql-text-sqlite 909.07 ms | 1.65x |
| CW-01 | N=20 | write | 833.28 ms | sql-text-sqlite 330.90 ms | 2.52x |
| CW-07 | N=20 | write | 430.92 ms | sql-text-sqlite 79.13 ms | 5.45x |
| CW-03 | N=20 | write | 330.97 ms | sql-text-sqlite 179.33 ms | 1.85x |
| CW-04 | N=20 | write | 231.24 ms | sql-text-sqlite 130.17 ms | 1.78x |
| XL-01..06 | 10MiB | create | 442.59 ms | sql-text-sqlite 373.91 ms | 1.18x |
| LL-01..03 | 32MiB | read | 77.01 ms | sql-text-sqlite 12.51 ms | 6.15x |
| CW-01 | N=5 | write | 80.79 ms | sql-text-sqlite 34.14 ms | 2.37x |
| RT-03 | all | create | 121.89 ms | sql-text-sqlite 78.12 ms | 1.56x |
| RT-03 | 1MiB/lf/random_bytes | create | 121.89 ms | sql-text-sqlite 78.12 ms | 1.56x |
| CW-04 | N=5 | write | 55.41 ms | sql-text-sqlite 19.30 ms | 2.87x |
| CR-05 | N=20 | search | 61.15 ms | sql-text-sqlite 25.94 ms | 2.36x |
| NS-02 | – | create | 31.68 ms | sql-text-sqlite 157 us | 201.38x |
| RT-01 | all | create | 48.35 ms | sql-text-sqlite 19.40 ms | 2.49x |
| RT-01 | 1MiB/lf/mixed | create | 48.35 ms | sql-text-sqlite 19.40 ms | 2.49x |
| RT-03 | 1MiB/lf/ascii | create | 37.30 ms | sql-text-sqlite 15.99 ms | 2.33x |
| RT-03 | 1MiB/lf/mixed | create | 62.86 ms | sql-text-sqlite 43.95 ms | 1.43x |
| XL-01..06 | 10MiB | read | 21.26 ms | sql-text-sqlite 5.26 ms | 4.04x |
| LL-01..03 | 1MiB | create | 27.14 ms | sql-text-sqlite 15.22 ms | 1.78x |
| CR-06 | N=50 | read_version | 13.89 ms | sql-text-sqlite 6.86 ms | 2.02x |
| RT-06 | – | create | 9.49 ms | sql-text-sqlite 3.63 ms | 2.62x |
| SR-01/02 | prefix | search | 11.21 ms | sql-text-sqlite 6.13 ms | 1.83x |
| RT-02 | 100KiB/no_trailing/mixed | create | 6.79 ms | sql-text-sqlite 2.26 ms | 3.00x |

## Footprint

| test | case | metric | textdb-sqlite | sql-text-sqlite |
|---|---|---|---|---|
| FP-01/02 | – | footprint_after_maintenance | 10,269,936 B | 16,826,304 B |
| FP-01/02 | – | footprint_after_maintenance_over_raw | 7.14x | 11.71x |
| FP-01/02 | after_0 | footprint_bytes | 7,459,016 B | 10,102,256 B |
| FP-01/02 | after_0 | footprint_over_raw | 5.06x | 6.85x |
| FP-01/02 | after_1200 | footprint_bytes | 9,478,344 B | 12,989,936 B |
| FP-01/02 | after_1200 | footprint_over_raw | 6.59x | 9.04x |
| FP-01/02 | after_300 | footprint_bytes | 8,118,472 B | 11,232,752 B |
| FP-01/02 | after_300 | footprint_over_raw | 5.55x | 7.67x |
| FP-01/02 | after_600 | footprint_bytes | 8,536,264 B | 11,757,040 B |
| FP-01/02 | after_600 | footprint_over_raw | 5.87x | 8.09x |
| FP-01/02 | after_900 | footprint_bytes | 9,404,616 B | 12,658,160 B |
| FP-01/02 | after_900 | footprint_over_raw | 6.51x | 8.76x |
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
| ME-05 | – | footprint_bytes | 9,618,328 B | 186,700,152 B |
| ME-05 | – | footprint_growth_bytes | 8,712,112 B | 185,781,576 B |
| ME-05 | – | footprint_over_raw | 32.44x | 629.65x |
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
| SR-04 | – | footprint_after_maintenance | 7,190,528 B | 7,759,656 B |
| SR-04 | – | footprint_growth_after_edits | 1,417,216 B | 1,587,144 B |
| SR-04 | – | footprint_growth_per_edit | 3,221 B | 3,607 B |
| SR-04 | – | footprint_over_raw | 8.18x | 8.37x |
| SR-05 | – | footprint_after_import | 6,770,768 B | 7,368,544 B |
| SR-05 | – | footprint_over_raw | 6.12x | 6.66x |
| XL-01..06 | 10MiB | footprint_after_create | 40,225,344 B | 48,540,032 B |
| XL-01..06 | 10MiB | footprint_growth_per_edit | 0 B | 10,495,590 B |
| XL-01..06 | 10MiB@100% | replace_footprint_growth | 0 B | 14,099,464 B |
| XL-01..06 | 10MiB@50% | replace_footprint_growth | 0 B | 10,493,952 B |
