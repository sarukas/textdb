# Optimisation candidates — textdb-sqlite

Source: `bench/out-five/results.jsonl` · baselines: fs, sql-text-sqlite, sql-text-pg

## By operation

Time is `p50 x calls`, summed over the tests that used the operation — an
estimate of where the run's time went, not a measured total.

| operation | calls | textdb-sqlite p50 | est. time | best baseline | ratio |
|---|---|---|---|---|---|
| read_version | 25426 | 3.80 ms | 96.65 s | sql-text-sqlite 1.14 ms | 3.33x |
| search | 2177 | 16.27 ms | 35.42 s | sql-text-sqlite 1.60 ms | 10.18x |
| replace | 10265 | 2.58 ms | 26.48 s | fs 380 us | 6.78x |
| create | 4849 | 1.00 ms | 4.86 s | fs 178 us | 5.61x |
| append | 1275 | 593 us | 755.84 ms | fs 169 us | 3.50x |
| rename | 1 | 2.52 ms | 2.52 ms | fs 13 us | 201.28x |
| history | 9 | 367 us | 3.30 ms | sql-text-sqlite 143 us | 2.56x |
| list | 2 | 1.37 ms | 2.74 ms | sql-text-sqlite 510 us | 2.69x |
| read_lines | 75741 | 575 us | 43.55 s | fs 1.52 ms | 0.38x |
| read | 4541610 | 8 us | 37.75 s | sql-text-sqlite 13 us | 0.66x |
| maintenance | 1 | 57.87 ms | 57.87 ms | sql-text-sqlite 79.02 ms | 0.73x |
| delete | 1 | 1.48 ms | 1.48 ms | fs 4.79 ms | 0.31x |

## Slowest cells relative to baseline

Per test/case p99, where the target is slower than the best baseline. Ranked by
the gap, so the top rows are where a fix changes the most.

| test | case | metric | textdb-sqlite | best baseline | ratio |
|---|---|---|---|---|---|
| CW-01 | N=50 | write | 2.34 s | fs 37.85 ms | 61.87x |
| LL-01..03 | 32MiB | create | 1.55 s | fs 174.94 ms | 8.86x |
| CW-05 | N=50 | write | 1.03 s | fs 26.16 ms | 39.42x |
| CW-04 | N=50 | write | 935.19 ms | sql-text-pg 18.45 ms | 50.69x |
| CW-02 | N=50 | write | 842.83 ms | fs 37.48 ms | 22.49x |
| CW-01 | N=20 | write | 532.43 ms | fs 9.56 ms | 55.71x |
| CW-03 | N=50 | write | 543.29 ms | fs 34.95 ms | 15.54x |
| XL-01..06 | 10MiB | create | 463.98 ms | fs 54.30 ms | 8.54x |
| CW-07 | N=20 | write | 335.83 ms | sql-text-pg 6.51 ms | 51.60x |
| CW-06 | N=20 | write | 331.87 ms | fs 7.33 ms | 45.30x |
| CW-03 | N=20 | write | 332.47 ms | fs 8.50 ms | 39.11x |
| ME-06 | N=20 | write | 231.35 ms | fs 10.13 ms | 22.84x |
| CW-02 | N=20 | write | 233.74 ms | fs 19.53 ms | 11.97x |
| CW-04 | N=20 | write | 181.04 ms | fs 6.77 ms | 26.75x |
| CW-05 | N=20 | write | 180.26 ms | fs 6.74 ms | 26.73x |
| RT-03 | 1MiB/lf/random_bytes | create | 130.75 ms | fs 351 us | 372.80x |
| RT-03 | all | create | 130.75 ms | fs 456 us | 286.62x |
| CW-03 | N=5 | write | 79.74 ms | fs 3.08 ms | 25.92x |
| LL-01..03 | 32MiB | read | 78.48 ms | fs 6.00 ms | 13.07x |
| RT-03 | 1MiB/lf/mixed | create | 63.31 ms | fs 343 us | 184.82x |
| CW-05 | N=5 | write | 54.54 ms | fs 913 us | 59.76x |
| CW-02 | N=5 | write | 56.12 ms | fs 2.58 ms | 21.77x |
| CW-04 | N=5 | write | 54.94 ms | fs 2.24 ms | 24.49x |
| RT-01 | 1MiB/lf/mixed | create | 50.82 ms | fs 355 us | 143.21x |
| RT-01 | all | create | 50.82 ms | fs 362 us | 140.34x |

## Footprint

| test | case | metric | textdb-sqlite | fs | sql-text-sqlite | sql-text-pg |
|---|---|---|---|---|---|---|
| FP-01/02 | – | footprint_after_maintenance | 10,269,936 B | – | 16,826,304 B | 7,282,688 B |
| FP-01/02 | – | footprint_after_maintenance_over_raw | 7.14x | – | 11.71x | 5.07x |
| FP-01/02 | after_0 | footprint_bytes | 7,459,016 B | 1,474,766 B | 10,102,256 B | 10,133,504 B |
| FP-01/02 | after_0 | footprint_over_raw | 5.06x | 1.00x | 6.85x | 6.87x |
| FP-01/02 | after_1200 | footprint_bytes | 9,478,344 B | 1,437,405 B | 12,989,936 B | 15,171,584 B |
| FP-01/02 | after_1200 | footprint_over_raw | 6.59x | 1.00x | 9.04x | 10.55x |
| FP-01/02 | after_300 | footprint_bytes | 8,118,472 B | 1,463,905 B | 11,232,752 B | 11,649,024 B |
| FP-01/02 | after_300 | footprint_over_raw | 5.55x | 1.00x | 7.67x | 7.96x |
| FP-01/02 | after_600 | footprint_bytes | 8,536,264 B | 1,454,040 B | 11,757,040 B | 12,836,864 B |
| FP-01/02 | after_600 | footprint_over_raw | 5.87x | 1.00x | 8.09x | 8.83x |
| FP-01/02 | after_900 | footprint_bytes | 9,404,616 B | 1,444,903 B | 12,658,160 B | 14,049,280 B |
| FP-01/02 | after_900 | footprint_over_raw | 6.51x | 1.00x | 8.76x | 9.72x |
| LL-04 | – | footprint_bytes | 14,649,448 B | 3,145,361 B | 84,385,000 B | – |
| LL-04 | – | footprint_growth_bytes | 0 B | 0 B | 67,476,224 B | – |
| LL-04 | – | footprint_over_raw | 4.66x | 1.00x | 26.83x | – |
| LL-05 | – | footprint_bytes | 12,209,296 B | 2,980,041 B | 82,992,040 B | 57,966,592 B |
| LL-05 | – | footprint_growth_bytes | 0 B | 0 B | 68,244,072 B | 54,960,128 B |
| LL-05 | – | footprint_over_raw | 4.10x | 1.00x | 27.85x | 19.45x |
| ME-01/02 | – | footprint_bytes | 5,759,056 B | 30,716 B | 23,782,128 B | 40,263,680 B |
| ME-01/02 | – | footprint_growth_bytes | 5,404,920 B | 0 B | 23,518,632 B | 40,009,728 B |
| ME-01/02 | – | footprint_over_raw | 187.49x | 1.00x | 774.26x | 1310.84x |
| ME-01/02 | after_150 | footprint_bytes | 4,636,344 B | 30,714 B | 8,544,336 B | 12,681,216 B |
| ME-01/02 | after_300 | footprint_bytes | 5,054,472 B | 30,717 B | 13,701,848 B | 22,396,928 B |
| ME-01/02 | after_450 | footprint_bytes | 5,390,344 B | 30,713 B | 18,723,544 B | 31,113,216 B |
| ME-01/02 | after_600 | footprint_bytes | 5,759,056 B | 30,716 B | 23,782,128 B | 40,263,680 B |
| ME-03 | – | footprint_bytes | 5,439,688 B | 41,410 B | 26,358,008 B | 34,512,896 B |
| ME-03 | – | footprint_growth_bytes | 5,085,552 B | 10,690 B | 26,094,512 B | 34,258,944 B |
| ME-03 | – | footprint_over_raw | 131.36x | 1.00x | 636.51x | 833.44x |
| ME-04 | – | footprint_bytes | 4,484,912 B | 30,649 B | 8,151,000 B | 12,681,216 B |
| ME-04 | – | footprint_growth_bytes | 4,130,776 B | 0 B | 7,887,504 B | 12,427,264 B |
| ME-04 | – | footprint_over_raw | 146.33x | 1.00x | 265.95x | 413.76x |
| ME-05 | – | footprint_bytes | 9,618,328 B | 296,516 B | 186,700,152 B | 217,169,920 B |
| ME-05 | – | footprint_growth_bytes | 8,712,112 B | 0 B | 185,781,576 B | 216,580,096 B |
| ME-05 | – | footprint_over_raw | 32.44x | 1.00x | 629.65x | 732.41x |
| RT-04 | – | footprint_bytes | 4,624,080 B | 26,667 B | 5,389,912 B | 4,816,896 B |
| RT-04 | – | footprint_growth_bytes | 4,269,944 B | 0 B | 5,126,416 B | 4,562,944 B |
| RT-04 | – | footprint_over_raw | 173.40x | 1.00x | 202.12x | 180.63x |
| RT-05 | – | footprint_bytes | 4,570,688 B | 30,151 B | 5,332,448 B | 5,447,680 B |
| RT-05 | – | footprint_growth_bytes | 4,216,552 B | 0 B | 5,068,952 B | 5,193,728 B |
| RT-05 | – | footprint_over_raw | 151.59x | 1.00x | 176.86x | 180.68x |
| RT-06 | – | footprint_after_import | 5,218,312 B | 368,682 B | 5,111,648 B | 3,080,192 B |
| RT-06 | – | footprint_over_raw | 14.15x | 1.00x | 13.86x | 8.35x |
| SR-01/02 | – | footprint_after_import | 6,770,768 B | 1,106,055 B | 7,368,544 B | 8,863,744 B |
| SR-01/02 | – | footprint_over_raw | 6.12x | 1.00x | 6.66x | 8.01x |
| SR-04 | – | footprint_after_import | 6,029,392 B | 737,368 B | 6,172,512 B | 5,980,160 B |
| SR-04 | – | footprint_after_maintenance | 7,190,528 B | – | 7,759,656 B | 4,423,680 B |
| SR-04 | – | footprint_growth_after_edits | 1,417,216 B | 0 B | 1,587,144 B | 3,481,600 B |
| SR-04 | – | footprint_growth_per_edit | 3,221 B | 0 B | 3,607 B | 7,913 B |
| SR-04 | – | footprint_over_raw | 8.18x | 1.00x | 8.37x | 8.11x |
| SR-05 | – | footprint_after_import | 6,770,768 B | 1,106,055 B | 7,368,544 B | 8,863,744 B |
| SR-05 | – | footprint_over_raw | 6.12x | 1.00x | 6.66x | 8.01x |
| XL-01..06 | 10MiB | footprint_after_create | 40,225,344 B | 10,485,760 B | 48,540,032 B | 10,985,472 B |
| XL-01..06 | 10MiB | footprint_growth_per_edit | 0 B | 0 B | 10,495,590 B | 11,116,544 B |
| XL-01..06 | 10MiB@100% | replace_footprint_growth | 0 B | 0 B | 14,099,464 B | 10,911,744 B |
| XL-01..06 | 10MiB@50% | replace_footprint_growth | 0 B | 0 B | 10,493,952 B | 10,903,552 B |

> 1 cell(s) published no timings because an accuracy check failed:
> - LL-04 / sql-text-pg
