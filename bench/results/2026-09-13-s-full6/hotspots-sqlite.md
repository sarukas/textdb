# Optimisation candidates — textdb-sqlite

Source: `bench/out-full2/results.jsonl` · baselines: fs, sql-text-sqlite, sql-text-pg, fs-git

## By operation

Time is `p50 x calls`, summed over the tests that used the operation — an
estimate of where the run's time went, not a measured total.

| operation | calls | textdb-sqlite p50 | est. time | best baseline | ratio |
|---|---|---|---|---|---|
| read_version | 19210 | 4.97 ms | 95.39 s | sql-text-sqlite 1.99 ms | 2.49x |
| search | 2325 | 15.25 ms | 35.46 s | sql-text-sqlite 2.06 ms | 7.40x |
| replace | 10988 | 2.25 ms | 24.73 s | fs 213 us | 10.59x |
| create | 4849 | 987 us | 4.79 s | fs 176 us | 5.59x |
| append | 1275 | 629 us | 802.18 ms | fs 134 us | 4.68x |
| rename | 1 | 2.54 ms | 2.54 ms | fs 12 us | 212.02x |
| history | 9 | 382 us | 3.43 ms | sql-text-sqlite 135 us | 2.83x |
| list | 2 | 1.46 ms | 2.93 ms | sql-text-sqlite 548 us | 2.67x |
| read_lines | 57264 | 638 us | 36.51 s | fs-git 1.57 ms | 0.41x |
| read | 3443386 | 11 us | 36.50 s | sql-text-sqlite 12 us | 0.90x |
| maintenance | 1 | 70.87 ms | 70.87 ms | sql-text-sqlite 81.64 ms | 0.87x |
| delete | 1 | 1.36 ms | 1.36 ms | fs 3.75 ms | 0.36x |

## Slowest cells relative to baseline

Per test/case p99, where the target is slower than the best baseline. Ranked by
the gap, so the top rows are where a fix changes the most.

| test | case | metric | textdb-sqlite | best baseline | ratio |
|---|---|---|---|---|---|
| LL-01..03 | 32MiB | create | 1.50 s | fs 137.84 ms | 10.88x |
| CW-01 | N=50 | write | 1.15 s | fs 32.95 ms | 34.80x |
| CW-02 | N=50 | write | 1.14 s | fs 39.75 ms | 28.61x |
| CW-05 | N=50 | write | 934.83 ms | fs 17.03 ms | 54.91x |
| CW-04 | N=50 | write | 932.03 ms | sql-text-pg 16.86 ms | 55.27x |
| CW-03 | N=50 | write | 530.57 ms | fs 24.37 ms | 21.77x |
| XL-01..06 | 10MiB | create | 437.25 ms | fs 55.46 ms | 7.88x |
| CW-07 | N=20 | write | 331.81 ms | sql-text-pg 6.28 ms | 52.84x |
| CW-04 | N=20 | write | 332.40 ms | sql-text-pg 7.76 ms | 42.83x |
| CW-01 | N=20 | write | 335.16 ms | fs 14.34 ms | 23.38x |
| CW-06 | N=20 | write | 251.12 ms | fs 5.28 ms | 47.56x |
| CW-02 | N=20 | write | 232.86 ms | fs 12.36 ms | 18.84x |
| CW-03 | N=20 | write | 180.10 ms | fs 5.87 ms | 30.67x |
| CW-05 | N=20 | write | 180.84 ms | fs 7.53 ms | 24.01x |
| ME-06 | N=20 | write | 181.09 ms | fs 8.05 ms | 22.50x |
| RT-03 | 1MiB/lf/random_bytes | create | 110.94 ms | fs 330 us | 336.06x |
| RT-03 | all | create | 110.94 ms | fs 471 us | 235.77x |
| CW-01 | N=5 | write | 81.36 ms | fs 2.74 ms | 29.73x |
| LL-01..03 | 32MiB | read | 79.35 ms | fs 5.88 ms | 13.51x |
| RT-03 | 1MiB/lf/mixed | create | 58.91 ms | fs 395 us | 149.12x |
| CW-03 | N=5 | write | 54.15 ms | fs 1.65 ms | 32.73x |
| CW-02 | N=5 | write | 54.58 ms | fs 2.44 ms | 22.38x |
| RT-01 | all | create | 48.76 ms | fs 269 us | 181.18x |
| RT-01 | 1MiB/lf/mixed | create | 48.76 ms | fs 269 us | 181.18x |
| LL-06 | 8MiB | create | 62.43 ms | fs 23.00 ms | 2.71x |

## Footprint

| test | case | metric | textdb-sqlite | fs | sql-text-sqlite | sql-text-pg | fs-git |
|---|---|---|---|---|---|---|---|
| FP-01/02 | – | footprint_after_maintenance | 10,269,936 B | – | 16,826,304 B | 7,282,688 B | – |
| FP-01/02 | – | footprint_after_maintenance_over_raw | 7.14x | – | 11.71x | 5.07x | – |
| FP-01/02 | after_0 | footprint_bytes | 7,459,016 B | 1,474,766 B | 10,102,256 B | 10,133,504 B | – |
| FP-01/02 | after_0 | footprint_over_raw | 5.06x | 1.00x | 6.85x | 6.87x | – |
| FP-01/02 | after_1200 | footprint_bytes | 9,478,344 B | 1,437,405 B | 12,989,936 B | 15,171,584 B | – |
| FP-01/02 | after_1200 | footprint_over_raw | 6.59x | 1.00x | 9.04x | 10.55x | – |
| FP-01/02 | after_300 | footprint_bytes | 8,118,472 B | 1,463,905 B | 11,232,752 B | 11,649,024 B | – |
| FP-01/02 | after_300 | footprint_over_raw | 5.55x | 1.00x | 7.67x | 7.96x | – |
| FP-01/02 | after_600 | footprint_bytes | 8,536,264 B | 1,454,040 B | 11,757,040 B | 12,836,864 B | – |
| FP-01/02 | after_600 | footprint_over_raw | 5.87x | 1.00x | 8.09x | 8.83x | – |
| FP-01/02 | after_900 | footprint_bytes | 9,404,616 B | 1,444,903 B | 12,658,160 B | 14,049,280 B | – |
| FP-01/02 | after_900 | footprint_over_raw | 6.51x | 1.00x | 8.76x | 9.72x | – |
| LL-04 | – | footprint_bytes | 14,649,448 B | 3,145,361 B | 84,385,000 B | – | 33,014,244 B |
| LL-04 | – | footprint_growth_bytes | 0 B | 0 B | 67,476,224 B | – | 28,418,916 B |
| LL-04 | – | footprint_over_raw | 4.66x | 1.00x | 26.83x | – | 10.50x |
| LL-05 | – | footprint_bytes | 12,209,296 B | 2,980,041 B | 82,992,040 B | 57,966,592 B | 28,746,511 B |
| LL-05 | – | footprint_growth_bytes | 0 B | 0 B | 68,244,072 B | 54,960,128 B | 24,313,501 B |
| LL-05 | – | footprint_over_raw | 4.10x | 1.00x | 27.85x | 19.45x | 9.65x |
| ME-01/02 | – | footprint_bytes | 5,759,056 B | 30,716 B | 23,782,128 B | 40,263,680 B | 8,298,545 B |
| ME-01/02 | – | footprint_growth_bytes | 5,404,920 B | 0 B | 23,518,632 B | 40,009,728 B | 8,225,491 B |
| ME-01/02 | – | footprint_over_raw | 187.49x | 1.00x | 774.26x | 1310.84x | 270.17x |
| ME-01/02 | after_150 | footprint_bytes | 4,636,344 B | 30,714 B | 8,544,336 B | 12,681,216 B | 2,129,342 B |
| ME-01/02 | after_300 | footprint_bytes | 5,054,472 B | 30,717 B | 13,701,848 B | 22,396,928 B | 4,185,746 B |
| ME-01/02 | after_450 | footprint_bytes | 5,390,344 B | 30,713 B | 18,723,544 B | 31,113,216 B | 6,242,145 B |
| ME-01/02 | after_600 | footprint_bytes | 5,759,056 B | 30,716 B | 23,782,128 B | 40,263,680 B | 8,298,545 B |
| ME-03 | – | footprint_bytes | 5,439,688 B | 41,410 B | 26,358,008 B | 42,942,464 B | 8,915,318 B |
| ME-03 | – | footprint_growth_bytes | 5,085,552 B | 10,690 B | 26,094,512 B | 42,688,512 B | 8,842,265 B |
| ME-03 | – | footprint_over_raw | 131.36x | 1.00x | 636.51x | 1037.01x | 215.29x |
| ME-04 | – | footprint_bytes | 4,484,912 B | 30,649 B | 8,151,000 B | 12,681,216 B | 2,126,285 B |
| ME-04 | – | footprint_growth_bytes | 4,130,776 B | 0 B | 7,887,504 B | 12,427,264 B | 2,053,231 B |
| ME-04 | – | footprint_over_raw | 146.33x | 1.00x | 265.95x | 413.76x | 69.38x |
| ME-05 | – | footprint_bytes | 9,618,328 B | 296,516 B | 186,700,152 B | 217,169,920 B | 74,268,064 B |
| ME-05 | – | footprint_growth_bytes | 8,712,112 B | 0 B | 185,781,576 B | 216,580,096 B | 73,796,151 B |
| ME-05 | – | footprint_over_raw | 32.44x | 1.00x | 629.65x | 732.41x | 250.47x |
| RT-04 | – | footprint_bytes | 4,624,080 B | 26,667 B | 5,389,912 B | 4,816,896 B | 815,697 B |
| RT-04 | – | footprint_growth_bytes | 4,269,944 B | 0 B | 5,126,416 B | 4,562,944 B | 742,644 B |
| RT-04 | – | footprint_over_raw | 173.40x | 1.00x | 202.12x | 180.63x | 30.59x |
| RT-05 | – | footprint_bytes | 4,570,688 B | 30,151 B | 5,332,448 B | 5,447,680 B | 885,199 B |
| RT-05 | – | footprint_growth_bytes | 4,216,552 B | 0 B | 5,068,952 B | 5,193,728 B | 812,142 B |
| RT-05 | – | footprint_over_raw | 151.59x | 1.00x | 176.86x | 180.68x | 29.36x |
| RT-06 | – | footprint_after_import | 5,218,312 B | 368,682 B | 5,111,648 B | 3,080,192 B | 1,219,721 B |
| RT-06 | – | footprint_over_raw | 14.15x | 1.00x | 13.86x | 8.35x | 3.31x |
| SR-01/02 | – | footprint_after_import | 6,770,768 B | 1,106,055 B | 7,368,544 B | 8,863,744 B | 3,797,131 B |
| SR-01/02 | – | footprint_over_raw | 6.12x | 1.00x | 6.66x | 8.01x | 3.43x |
| SR-04 | – | footprint_after_import | 6,029,392 B | 737,368 B | 6,172,512 B | 5,980,160 B | 2,486,021 B |
| SR-04 | – | footprint_after_maintenance | 7,190,528 B | – | 7,759,656 B | 4,423,680 B | 2,118,855 B |
| SR-04 | – | footprint_growth_after_edits | 1,417,216 B | 0 B | 1,587,144 B | 3,481,600 B | 1,271,982 B |
| SR-04 | – | footprint_growth_per_edit | 3,221 B | 0 B | 3,607 B | 7,913 B | 2,891 B |
| SR-04 | – | footprint_over_raw | 8.18x | 1.00x | 8.37x | 8.11x | 3.37x |
| SR-05 | – | footprint_after_import | 6,770,768 B | 1,106,055 B | 7,368,544 B | 8,863,744 B | 3,797,108 B |
| SR-05 | – | footprint_over_raw | 6.12x | 1.00x | 6.66x | 8.01x | 3.43x |
| XL-01..06 | 10MiB | footprint_after_create | 40,225,344 B | 10,485,760 B | 48,540,032 B | 10,985,472 B | 14,735,754 B |
| XL-01..06 | 10MiB | footprint_growth_per_edit | 0 B | 0 B | 10,495,590 B | 11,116,544 B | 4,249,727 B |
| XL-01..06 | 10MiB@100% | replace_footprint_growth | 0 B | 0 B | 14,099,464 B | 10,911,744 B | 4,249,790 B |
| XL-01..06 | 10MiB@50% | replace_footprint_growth | 0 B | 0 B | 10,493,952 B | 10,903,552 B | 4,249,623 B |

> 2 cell(s) published no timings because an accuracy check failed:
> - LL-04 / sql-text-pg
> - NS-01 / fs-git
