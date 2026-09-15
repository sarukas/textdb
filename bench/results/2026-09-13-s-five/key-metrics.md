
### Claim 1 — O(edit) writes: write amplification (bytes written / bytes changed) and latency of a 3-line replace

| cell | fs | sql-text-sqlite | sql-text-pg | textdb-sqlite | textdb-pg |
|---|---|---|---|---|---|
| XL-04 replace @50 % of 10MiB: write amplification | 43,150.3× | 199,584.7× | 46,796.0× | 441.0× | 81.8× |
| XL-04 replace @50 % of ops: write amplification | – | – | – | – | – |
| XL-04 replace @50 % of verify_post: write amplification | – | – | – | – | – |
| XL-04 replace @50 % of verify_pre: write amplification | – | – | – | – | – |
| XL-04 replace @50 % of 10MiB: p50 | 85.05 ms | 456.88 ms | 2019.70 ms | 45.05 ms | 215.56 ms |
| XL-04 replace @50 % of ops: p50 | – | – | – | – | – |
| XL-04 replace @50 % of verify_post: p50 | – | – | – | – | – |
| XL-04 replace @50 % of verify_pre: p50 | – | – | – | – | – |
| XL-05 footprint growth per edit (10MiB) | 0 B | 10.5 MB | 11.1 MB | 0 B | 11.5 KB |
| XL-05 footprint growth per edit (ops) | – | – | – | – | – |
| XL-05 footprint growth per edit (verify_post) | – | – | – | – | – |
| XL-05 footprint growth per edit (verify_pre) | – | – | – | – | – |
| LL-02 replace 10 B in a 1 MiB single line: write amplification | – | – | – | – | – |
| LL-02 textdb leaves changed (1 MiB line) | – | – | – | – | – |
| LL-04 JSON 10 MiB: leaves unchanged per edit (min fraction) | N/A | – | – | 99.9 % | 99.9 % |
| ME-04 alternate ends: leaves changed per edit (max) | N/A | – | – | 1 | 1 |
| ME-01 counter ×2000 on 100 KiB: replace p50 | 159 µs | 1.24 ms | 5.07 ms | 813 µs | 3.55 ms |
| ME-01 write amplification | 2,092.6× | 12,142.3× | 4,935.6× | 7,996.5× | 765.1× |
| ME-01 footprint / raw after 2000 edits | 1.0× | 774.3× | 1,310.8× | 187.5× | 168.3× |
| ME-01 textdb chunk bytes (content layer only) | N/A | – | – | 436.8 KB | 436.8 KB |

### Claim 2 — conflict rate and lost updates under concurrent writers (base = last seen version)

| cell | fs | sql-text-sqlite | sql-text-pg | textdb-sqlite | textdb-pg |
|---|---|---|---|---|---|
| CW-01 N=5: committed_direct | 38 | 38 | 10 | 26 | 1 |
| CW-01 N=5: committed_rebased | 0 | 0 | 0 | 19 | 44 |
| CW-01 N=5: absorbed_identical | 0 | 0 | 0 | 0 | 0 |
| CW-01 N=5: conflict | 7 | 7 | 35 | 0 | 0 |
| CW-01 N=5: contention | 0 | 0 | 0 | 0 | 0 |
| CW-01 N=5: error | 0 | 0 | 0 | 0 | 0 |
| CW-01 N=5: lost_updates | 19 | 0 | 0 | 0 | 0 |
| CW-01 N=20: committed_direct | 159 | 120 | 17 | 88 | 2 |
| CW-01 N=20: committed_rebased | 0 | 6 | 0 | 92 | 177 |
| CW-01 N=20: absorbed_identical | 0 | 0 | 0 | 0 | 0 |
| CW-01 N=20: conflict | 21 | 54 | 163 | 0 | 0 |
| CW-01 N=20: contention | 0 | 0 | 0 | 0 | 0 |
| CW-01 N=20: error | 0 | 0 | 0 | 0 | 1 |
| CW-01 N=20: lost_updates | 139 | 0 | 0 | 0 | 0 |
| CW-01 N=50: committed_direct | 389 | 246 | 24 | 264 | 1 |
| CW-01 N=50: committed_rebased | 0 | 14 | 0 | 186 | 353 |
| CW-01 N=50: absorbed_identical | 0 | 0 | 0 | 0 | 0 |
| CW-01 N=50: conflict | 61 | 190 | 426 | 0 | 0 |
| CW-01 N=50: contention | 0 | 0 | 0 | 0 | 3 |
| CW-01 N=50: error | 0 | 0 | 0 | 0 | 93 |
| CW-01 N=50: lost_updates | 343 | 0 | 0 | 0 | 0 |
| CW-02 N=5: committed_direct | 39 | 31 | 10 | 23 | 1 |
| CW-02 N=5: committed_rebased | 0 | 0 | 0 | 20 | 35 |
| CW-02 N=5: absorbed_identical | 0 | 0 | 0 | 0 | 0 |
| CW-02 N=5: conflict | 6 | 14 | 35 | 2 | 9 |
| CW-02 N=5: contention | 0 | 0 | 0 | 0 | 0 |
| CW-02 N=5: error | 0 | 0 | 0 | 0 | 0 |
| CW-02 N=5: lost_updates | 1 | 0 | 0 | 0 | 0 |
| CW-02 N=20: committed_direct | 155 | 110 | 18 | 69 | 3 |
| CW-02 N=20: committed_rebased | 0 | 7 | 0 | 102 | 151 |
| CW-02 N=20: absorbed_identical | 0 | 0 | 0 | 0 | 0 |
| CW-02 N=20: conflict | 25 | 63 | 162 | 9 | 21 |
| CW-02 N=20: contention | 0 | 0 | 0 | 0 | 0 |
| CW-02 N=20: error | 0 | 0 | 0 | 0 | 5 |
| CW-02 N=20: lost_updates | 25 | 0 | 0 | 0 | 0 |
| CW-02 N=50: committed_direct | 376 | 267 | 21 | 146 | 6 |
| CW-02 N=50: committed_rebased | 0 | 10 | 0 | 278 | 311 |
| CW-02 N=50: absorbed_identical | 0 | 0 | 0 | 0 | 0 |
| CW-02 N=50: conflict | 70 | 169 | 425 | 22 | 42 |
| CW-02 N=50: contention | 0 | 0 | 0 | 0 | 0 |
| CW-02 N=50: error | 0 | 0 | 0 | 0 | 87 |
| CW-02 N=50: lost_updates | 76 | 0 | 0 | 0 | 0 |
| CW-03 N=5: committed_direct | 34 | 38 | 11 | 38 | 13 |
| CW-03 N=5: committed_rebased | 0 | 0 | 0 | 0 | 0 |
| CW-03 N=5: absorbed_identical | 0 | 0 | 0 | 1 | 30 |
| CW-03 N=5: conflict | 11 | 7 | 34 | 6 | 2 |
| CW-03 N=5: contention | 0 | 0 | 0 | 0 | 0 |
| CW-03 N=5: error | 0 | 0 | 0 | 0 | 0 |
| CW-03 N=5: lost_updates | 22 | 0 | 0 | 0 | 0 |
| CW-03 N=20: committed_direct | 129 | 125 | 16 | 105 | 12 |
| CW-03 N=20: committed_rebased | 0 | 4 | 0 | 0 | 0 |
| CW-03 N=20: absorbed_identical | 0 | 0 | 0 | 20 | 166 |
| CW-03 N=20: conflict | 51 | 51 | 164 | 55 | 2 |
| CW-03 N=20: contention | 0 | 0 | 0 | 0 | 0 |
| CW-03 N=20: error | 0 | 0 | 0 | 0 | 0 |
| CW-03 N=20: lost_updates | 117 | 0 | 0 | 0 | 0 |
| CW-03 N=50: committed_direct | 314 | 301 | 27 | 216 | 23 |
| CW-03 N=50: committed_rebased | 0 | 16 | 0 | 0 | 0 |
| CW-03 N=50: absorbed_identical | 0 | 0 | 0 | 54 | 375 |
| CW-03 N=50: conflict | 136 | 133 | 423 | 180 | 52 |
| CW-03 N=50: contention | 0 | 0 | 0 | 0 | 0 |
| CW-03 N=50: error | 0 | 0 | 0 | 0 | 0 |
| CW-03 N=50: lost_updates | 297 | 0 | 0 | 0 | 0 |
| ME-06 N=20: committed_direct | 166 | 171 | 26 | 134 | 19 |
| ME-06 N=20: committed_rebased | 0 | 10 | 0 | 0 | 0 |
| ME-06 N=20: absorbed_identical | 0 | 0 | 0 | 28 | 219 |
| ME-06 N=20: conflict | 74 | 59 | 214 | 78 | 2 |
| ME-06 N=20: contention | 0 | 0 | 0 | 0 | 0 |
| ME-06 N=20: error | 0 | 0 | 0 | 0 | 0 |
| ME-06 N=20: lost_updates | 127 | 0 | 0 | 0 | 0 |
| CW-01 N=20: write p99 | 9.56 ms | 432.01 ms | 48.71 ms | 532.43 ms | 1134.84 ms |
| CW-02 N=20: write p99 | 19.53 ms | 180.69 ms | 47.05 ms | 233.74 ms | 1324.66 ms |
| CW-03 N=20: write p99 | 8.50 ms | 330.27 ms | 57.25 ms | 332.47 ms | 53.16 ms |
| ME-06 N=20: write p99 | 10.13 ms | 530.07 ms | 56.60 ms | 231.35 ms | 53.71 ms |
| CW-01 N=20: throughput ops/s | 6,773 | 277 | 680 | 239 | 85 |
| CW-02 N=20: throughput ops/s | 5,849 | 518 | 721 | 395 | 102 |
| CW-03 N=20: throughput ops/s | 7,545 | 318 | 651 | 325 | 475 |
| ME-06 N=20: throughput ops/s | 8,983 | 365 | 670 | 520 | 497 |
| CW-04 Zipf files N=20: conflict | 0 | 18 | 42 | 0 | 4 |
| CW-04 Zipf files N=20: committed_rebased | 0 | 2 | 0 | 18 | 52 |
| CW-05 append N=20: lost updates | 165 | 0 | 0 | 0 | 0 |
| CW-05 append N=20: append p50 | 226 µs | 1.42 ms | 131.49 ms | 548 µs | 24.29 ms |
| CW-06 rename race N=20: error (write failed by rename) | 0 | 0 | 0 | 0 | 0 |
| CW-06 rename race N=20: lost updates | 69 | 0 | 0 | 0 | 0 |
| CW-07 fs-git 20 s: contention (index.lock) | 0 | 0 | 0 | 0 | 6 |

### Claim 3 — insert-only index: footprint growth per edit with search index, maintenance

| cell | fs | sql-text-sqlite | sql-text-pg | textdb-sqlite | textdb-pg |
|---|---|---|---|---|---|
| SR-04 footprint growth per edit (2000 edits, 2000 × 4 KiB docs) | 0 B | 3.6 KB | 7.9 KB | 3.2 KB | 7.1 KB |
| SR-04 bytes written for the edits | 509.5 KB | 26.0 MB | 0 B | 49.7 MB | 6.2 MB |
| SR-04 maintenance time | N/A | 0.0121 | 0.0877 | 0.0291 | 0.128 |
| SR-01 import 3000 × 4 KiB | 0.113 | 0.288 | 0.504 | 0.557 | 1.35 |
| SR-01 footprint / raw after import | 1.0× | 6.7× | 8.0× | 6.1× | 7.9× |
| FP-01 footprint / raw after 4000 Zipf edits on 2000 files | N/A | – | – | – | – |
| FP-02 footprint / raw after maintenance | N/A | 11.7× | 5.1× | 7.1× | 8.0× |
| FP-02 maintenance time | N/A | 0.079 | 0.115 | 0.0579 | 0.228 |

### Search — latency and recall/precision vs the reference tokenizer (3000 docs)

| cell | fs | sql-text-sqlite | sql-text-pg | textdb-sqlite | textdb-pg |
|---|---|---|---|---|---|
| SR-02 single: p50 | 10.24 ms | 567 µs | 3.62 ms | 1.04 ms | 9.72 ms |
| SR-02 and2: p50 | 18.33 ms | 105 µs | 2.79 ms | 619 µs | 9.09 ms |
| SR-02 phrase: p50 | 8.96 ms | 134 µs | 2.86 ms | 369 µs | 5.22 ms |
| SR-02 prefix: p50 | 11.41 ms | 1.19 ms | 4.65 ms | 2.21 ms | 15.71 ms |
| SR-02 single: recall | 100.0 % | 100.0 % | 99.8 % | 100.0 % | 99.8 % |
| SR-02 and2: recall | 100.0 % | 100.0 % | 100.0 % | 100.0 % | 100.0 % |
| SR-02 phrase: recall | 88.9 % | 100.0 % | 89.6 % | 100.0 % | 89.6 % |
| SR-02 prefix: recall | 100.0 % | 100.0 % | 99.7 % | 100.0 % | 99.7 % |
| SR-02 single: precision | 100.0 % | 100.0 % | 100.0 % | 100.0 % | 100.0 % |
| SR-02 and2: precision | 100.0 % | 100.0 % | 100.0 % | 100.0 % | 100.0 % |
| SR-02 phrase: precision | 100.0 % | 100.0 % | 100.0 % | 100.0 % | 100.0 % |
| SR-02 prefix: precision | 100.0 % | 100.0 % | 100.0 % | 100.0 % | 100.0 % |
| SR-05 prefix-restricted AND: p50 | 10.61 ms | 40 µs | 281 µs | 470 µs | 1.27 ms |
| CR-05 20 concurrent searchers: throughput | 180 | 3,769 | 1,221 | 927 | 213 |

### Reads — full and fragment reads, history

| cell | fs | sql-text-sqlite | sql-text-pg | textdb-sqlite | textdb-pg |
|---|---|---|---|---|---|
| XL-02 full read 10MiB: p50 | 5.19 ms | 3.56 ms | 49.90 ms | 21.13 ms | 227.44 ms |
| XL-02 full read ops: p50 | – | – | – | – | – |
| XL-02 full read verify_post: p50 | – | – | – | – | – |
| XL-02 full read verify_pre: p50 | – | – | – | – | – |
| XL-02 full read 10MiB: MB/s | 2,018 | 2,941 | 210 | 496 | 46 |
| XL-02 full read ops: MB/s | – | – | – | – | – |
| XL-02 full read verify_post: MB/s | – | – | – | – | – |
| XL-02 full read verify_pre: MB/s | – | – | – | – | – |
| XL-03 read_lines 50 @50 % of 10MiB: p50 | 5.67 ms | 7.88 ms | 58.32 ms | 142 µs | 771 µs |
| XL-03 read_lines 50 @50 % of ops: p50 | – | – | – | – | – |
| XL-03 read_lines 50 @50 % of verify_post: p50 | – | – | – | – | – |
| XL-03 read_lines 50 @50 % of verify_pre: p50 | – | – | – | – | – |
| XL-06 read_version(v1) after edits (10MiB) | N/A | 31.95 ms | 52.97 ms | 21.02 ms | 232.14 ms |
| XL-06 read_version(v1) after edits (ops) | – | – | – | – | – |
| XL-06 read_version(v1) after edits (verify_post) | – | – | – | – | – |
| XL-06 read_version(v1) after edits (verify_pre) | – | – | – | – | – |
| CR-01 N=1 read 100 KiB: p50 | 3 µs | 8 µs | 314 µs | 9 µs | 1.08 ms |
| CR-01 N=100 read 100 KiB: throughput | 858,784 | 101,632 | 11,688 | 210,556 | 2,919 |
| CR-01 N=100 read 100 KiB: p99 | 8 µs | 4.67 ms | 25.04 ms | 6.05 ms | 121.64 ms |
| CR-03 N=100 readers + writer: reader p99 | 8 µs | 3.98 ms | 21.64 ms | 6.23 ms | 71.42 ms |
| CR-03 N=100 torn reads | 0 | 0 | 0 | 0 | 0 |
| CR-04 N=100 read_lines on 32 MiB: throughput | 2,335 | 618 | 40 | 14,096 | 5,301 |
| CR-04 N=100 read_lines on 32 MiB: p50 | 1.53 ms | 51.24 ms | 2530.93 ms | 6.85 ms | 15.56 ms |
| CR-06 N=50 read_version (200 versions): p50 | N/A | 1.14 ms | 5.51 ms | 3.82 ms | 12.37 ms |
| ME-02 read_version p50 after 2000 versions | N/A | 262 µs | 475 µs | 20 µs | 1.07 ms |
| ME-05 history p50 after 3000 versions | N/A | 231 µs | 1.15 ms | 737 µs | 1.23 ms |

### Namespace and round trip

| cell | fs | sql-text-sqlite | sql-text-pg | textdb-sqlite | textdb-pg |
|---|---|---|---|---|---|
| RT-01 all sizes identical (1 = pass) | 1 | 1 | 1 | 1 | 1 |
| RT-03 random bytes 1 MiB identical | 1 | 1 | N/A | 1 | N/A |
| RT-04 300 random edits: versions identical | N/A | 1 | 1 | 1 | 1 |
| RT-05 all historical versions identical | N/A | 1 | 1 | 1 | 1 |
| NS-01 create p99 among 5000 files in one folder | 166 µs | 651 µs | 487 µs | 512 µs | 1.26 ms |
| NS-01 list 5000 entries | 3.37 ms | 984 µs | 2.73 ms | 2.65 ms | 2.27 ms |
| NS-02 depth 1000 create | 35.52 ms | 221 µs | 623 µs | 30.10 ms | 77.24 ms |
| NS-03 rename folder with 5000 descendants | – | – | – | – | – |
| NS-03 versions preserved | – | – | – | – | – |
| NS-04 read_version after folder delete | N/A | 1 | 1 | 1 | 1 |
| NS-05 255-byte name identical | – | 1 | 1 | 1 | 1 |
| NS-05 4 KiB path identical | 1 | 1 | 1 | 1 | 1 |
| LL-06 UTF-8 intact after multibyte replace | – | – | – | – | – |
