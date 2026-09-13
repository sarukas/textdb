# Optimisation candidates — textdb-pg

Source: `bench/out-full2/results.jsonl` · baselines: fs, sql-text-sqlite, sql-text-pg, fs-git

## By operation

Time is `p50 x calls`, summed over the tests that used the operation — an
estimate of where the run's time went, not a measured total.

| operation | calls | textdb-pg p50 | est. time | best baseline | ratio |
|---|---|---|---|---|---|
| replace | 10616 | 29.60 ms | 314.28 s | fs 213 us | 139.24x |
| read | 69905 | 2.78 ms | 194.44 s | sql-text-sqlite 12 us | 235.43x |
| read_version | 3635 | 11.96 ms | 43.46 s | sql-text-sqlite 1.99 ms | 6.00x |
| search | 728 | 47.72 ms | 34.74 s | sql-text-sqlite 2.06 ms | 23.16x |
| create | 4848 | 5.86 ms | 28.39 s | fs 176 us | 33.18x |
| append | 1275 | 19.30 ms | 24.61 s | fs 134 us | 143.56x |
| read_lines | 24422 | 2.37 ms | 57.91 s | fs-git 1.57 ms | 1.51x |
| maintenance | 1 | 256.18 ms | 256.18 ms | sql-text-sqlite 81.64 ms | 3.14x |
| rename | 1 | 15.67 ms | 15.67 ms | fs 12 us | 1305.40x |
| history | 9 | 884 us | 7.95 ms | sql-text-sqlite 135 us | 6.56x |
| delete | 1 | 6.25 ms | 6.25 ms | fs 3.75 ms | 1.67x |
| list | 2 | 1.35 ms | 2.71 ms | sql-text-sqlite 548 us | 2.47x |

## Slowest cells relative to baseline

Per test/case p99, where the target is slower than the best baseline. Ranked by
the gap, so the top rows are where a fix changes the most.

| test | case | metric | textdb-pg | best baseline | ratio |
|---|---|---|---|---|---|
| CW-02 | N=50 | write | 60.94 s | fs 39.75 ms | 1533.24x |
| CW-01 | N=50 | write | 45.96 s | fs 32.95 ms | 1394.91x |
| CW-02 | N=20 | write | 17.03 s | fs 12.36 ms | 1378.13x |
| LL-01..03 | 32MiB | create | 9.24 s | fs 137.84 ms | 67.03x |
| XL-01..06 | 10MiB | create | 2.73 s | fs 55.46 ms | 49.23x |
| LL-01..03 | 32MiB | read | 922.93 ms | fs 5.88 ms | 157.08x |
| LL-01..03 | 32MiB | read_lines_1_1 | 900.61 ms | fs 51.74 ms | 17.41x |
| CW-05 | N=50 | write | 707.84 ms | fs 17.03 ms | 41.58x |
| CR-06 | N=50 | read_version | 680.05 ms | sql-text-sqlite 7.34 ms | 92.69x |
| CR-03 | N=100 | writer_replace | 653.80 ms | fs 6.23 ms | 104.88x |
| CW-01 | N=20 | write | 402.00 ms | fs 14.34 ms | 28.04x |
| RT-03 | 1MiB/lf/mixed | create | 317.14 ms | fs 395 us | 802.84x |
| RT-03 | all | create | 317.14 ms | fs 471 us | 674.00x |
| RT-01 | all | create | 275.66 ms | fs 269 us | 1024.22x |
| RT-01 | 1MiB/lf/mixed | create | 275.66 ms | fs 269 us | 1024.22x |
| RT-03 | 1MiB/lf/ascii | create | 275.45 ms | fs 471 us | 585.40x |
| CR-05 | N=20 | search | 269.83 ms | fs-git 22.97 ms | 11.75x |
| LL-01..03 | 1MiB | create | 234.13 ms | fs 345 us | 679.32x |
| CW-04 | N=50 | write | 242.30 ms | sql-text-pg 16.86 ms | 14.37x |
| XL-01..06 | 10MiB | read | 227.04 ms | sql-text-sqlite 4.77 ms | 47.61x |
| XL-01..06 | 10MiB | read_version_v1 | 218.30 ms | sql-text-sqlite 25.76 ms | 8.47x |
| XL-01..06 | 10MiB@100% | replace | 212.62 ms | fs 38.11 ms | 5.58x |
| XL-01..06 | 10MiB | seq_replace | 213.39 ms | fs 39.91 ms | 5.35x |
| CW-05 | N=20 | write | 180.47 ms | fs 7.53 ms | 23.96x |
| CR-01 | N=100 | read | 153.33 ms | fs-git 8 us | 19068.51x |

## Footprint

| test | case | metric | textdb-pg | fs | sql-text-sqlite | sql-text-pg | fs-git |
|---|---|---|---|---|---|---|---|
| FP-01/02 | – | footprint_after_maintenance | 11,427,840 B | – | 16,826,304 B | 7,282,688 B | – |
| FP-01/02 | – | footprint_after_maintenance_over_raw | 7.95x | – | 11.71x | 5.07x | – |
| FP-01/02 | after_0 | footprint_bytes | 11,304,960 B | 1,474,766 B | 10,102,256 B | 10,133,504 B | – |
| FP-01/02 | after_0 | footprint_over_raw | 7.67x | 1.00x | 6.85x | 6.87x | – |
| FP-01/02 | after_1200 | footprint_bytes | 15,155,200 B | 1,437,405 B | 12,989,936 B | 15,171,584 B | – |
| FP-01/02 | after_1200 | footprint_over_raw | 10.54x | 1.00x | 9.04x | 10.55x | – |
| FP-01/02 | after_300 | footprint_bytes | 12,337,152 B | 1,463,905 B | 11,232,752 B | 11,649,024 B | – |
| FP-01/02 | after_300 | footprint_over_raw | 8.43x | 1.00x | 7.67x | 7.96x | – |
| FP-01/02 | after_600 | footprint_bytes | 13,328,384 B | 1,454,040 B | 11,757,040 B | 12,836,864 B | – |
| FP-01/02 | after_600 | footprint_over_raw | 9.17x | 1.00x | 8.09x | 8.83x | – |
| FP-01/02 | after_900 | footprint_bytes | 14,245,888 B | 1,444,903 B | 12,658,160 B | 14,049,280 B | – |
| FP-01/02 | after_900 | footprint_over_raw | 9.86x | 1.00x | 8.76x | 9.72x | – |
| LL-04 | – | footprint_bytes | 18,538,496 B | 3,145,361 B | 84,385,000 B | – | 33,014,244 B |
| LL-04 | – | footprint_growth_bytes | 229,376 B | 0 B | 67,476,224 B | – | 28,418,916 B |
| LL-04 | – | footprint_over_raw | 5.89x | 1.00x | 26.83x | – | 10.50x |
| LL-05 | – | footprint_bytes | 16,171,008 B | 2,980,041 B | 82,992,040 B | 57,966,592 B | 28,746,511 B |
| LL-05 | – | footprint_growth_bytes | 270,336 B | 0 B | 68,244,072 B | 54,960,128 B | 24,313,501 B |
| LL-05 | – | footprint_over_raw | 5.43x | 1.00x | 27.85x | 19.45x | 9.65x |
| ME-01/02 | – | footprint_bytes | 5,169,152 B | 30,716 B | 23,782,128 B | 40,263,680 B | 8,298,545 B |
| ME-01/02 | – | footprint_growth_bytes | 4,571,136 B | 0 B | 23,518,632 B | 40,009,728 B | 8,225,491 B |
| ME-01/02 | – | footprint_over_raw | 168.29x | 1.00x | 774.26x | 1310.84x | 270.17x |
| ME-01/02 | after_150 | footprint_bytes | 1,810,432 B | 30,714 B | 8,544,336 B | 12,681,216 B | 2,129,342 B |
| ME-01/02 | after_300 | footprint_bytes | 2,949,120 B | 30,717 B | 13,701,848 B | 22,396,928 B | 4,185,746 B |
| ME-01/02 | after_450 | footprint_bytes | 4,046,848 B | 30,713 B | 18,723,544 B | 31,113,216 B | 6,242,145 B |
| ME-01/02 | after_600 | footprint_bytes | 5,169,152 B | 30,716 B | 23,782,128 B | 40,263,680 B | 8,298,545 B |
| ME-03 | – | footprint_bytes | 3,694,592 B | 41,410 B | 26,358,008 B | 42,942,464 B | 8,915,318 B |
| ME-03 | – | footprint_growth_bytes | 3,096,576 B | 10,690 B | 26,094,512 B | 42,688,512 B | 8,842,265 B |
| ME-03 | – | footprint_over_raw | 89.22x | 1.00x | 636.51x | 1037.01x | 215.29x |
| ME-04 | – | footprint_bytes | 1,179,648 B | 30,649 B | 8,151,000 B | 12,681,216 B | 2,126,285 B |
| ME-04 | – | footprint_growth_bytes | 581,632 B | 0 B | 7,887,504 B | 12,427,264 B | 2,053,231 B |
| ME-04 | – | footprint_over_raw | 38.49x | 1.00x | 265.95x | 413.76x | 69.38x |
| ME-05 | – | footprint_bytes | 24,788,992 B | 296,516 B | 186,700,152 B | 217,169,920 B | 74,268,064 B |
| ME-05 | – | footprint_growth_bytes | 22,315,008 B | 0 B | 185,781,576 B | 216,580,096 B | 73,796,151 B |
| ME-05 | – | footprint_over_raw | 83.60x | 1.00x | 629.65x | 732.41x | 250.47x |
| RT-04 | – | footprint_bytes | 1,318,912 B | 26,667 B | 5,389,912 B | 4,816,896 B | 815,697 B |
| RT-04 | – | footprint_growth_bytes | 720,896 B | 0 B | 5,126,416 B | 4,562,944 B | 742,644 B |
| RT-04 | – | footprint_over_raw | 49.46x | 1.00x | 202.12x | 180.63x | 30.59x |
| RT-05 | – | footprint_bytes | 1,425,408 B | 30,151 B | 5,332,448 B | 5,447,680 B | 885,199 B |
| RT-05 | – | footprint_growth_bytes | 827,392 B | 0 B | 5,068,952 B | 5,193,728 B | 812,142 B |
| RT-05 | – | footprint_over_raw | 47.28x | 1.00x | 176.86x | 180.68x | 29.36x |
| RT-06 | – | footprint_after_import | 3,309,568 B | 368,682 B | 5,111,648 B | 3,080,192 B | 1,219,721 B |
| RT-06 | – | footprint_over_raw | 8.98x | 1.00x | 13.86x | 8.35x | 3.31x |
| SR-01/02 | – | footprint_after_import | 8,724,480 B | 1,106,055 B | 7,368,544 B | 8,863,744 B | 3,797,131 B |
| SR-01/02 | – | footprint_over_raw | 7.89x | 1.00x | 6.66x | 8.01x | 3.43x |
| SR-04 | – | footprint_after_import | 6,045,696 B | 737,368 B | 6,172,512 B | 5,980,160 B | 2,486,021 B |
| SR-04 | – | footprint_after_maintenance | 6,160,384 B | – | 7,759,656 B | 4,423,680 B | 2,118,855 B |
| SR-04 | – | footprint_growth_after_edits | 3,145,728 B | 0 B | 1,587,144 B | 3,481,600 B | 1,271,982 B |
| SR-04 | – | footprint_growth_per_edit | 7,149 B | 0 B | 3,607 B | 7,913 B | 2,891 B |
| SR-04 | – | footprint_over_raw | 8.20x | 1.00x | 8.37x | 8.11x | 3.37x |
| SR-05 | – | footprint_after_import | 8,724,480 B | 1,106,055 B | 7,368,544 B | 8,863,744 B | 3,797,108 B |
| SR-05 | – | footprint_over_raw | 7.89x | 1.00x | 6.66x | 8.01x | 3.43x |
| XL-01..06 | 10MiB | footprint_after_create | 42,344,448 B | 10,485,760 B | 48,540,032 B | 10,985,472 B | 14,735,754 B |
| XL-01..06 | 10MiB | footprint_growth_per_edit | 11,469 B | 0 B | 10,495,590 B | 11,116,544 B | 4,249,727 B |
| XL-01..06 | 10MiB@100% | replace_footprint_growth | 16,384 B | 0 B | 14,099,464 B | 10,911,744 B | 4,249,790 B |
| XL-01..06 | 10MiB@50% | replace_footprint_growth | 8,192 B | 0 B | 10,493,952 B | 10,903,552 B | 4,249,623 B |

> 2 cell(s) published no timings because an accuracy check failed:
> - LL-04 / sql-text-pg
> - NS-01 / fs-git
