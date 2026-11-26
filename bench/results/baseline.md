# aoloxide vs AOLserver vs axum: 2026-11-26T20:06Z

* Host: 4 CPUs, Intel(R) Xeon(R) Processor @ 2.10GHz, Linux 6.18.44-fc-v880
* Servers pinned to CPUs 0-1, wrk (2 threads) to CPUs 2-3
* 10s per point after 2s warmup; keep-alive (`Connection: keep-alive` sent); conn threads (both nsds): 20
* aoloxide f7396c0, wrk debian/4.1.0-4build2, rustc 1.97.0 (2d8144b78 2026-07-07)
* AOLserver: C build from bench/build-aolserver.sh at /opt/aolserver-bench, Tcl 8.6.14
* Peak RSS: aoloxide 35 MiB aolserver 24 MiB axum 34 MiB axum-cached 13 MiB

## Throughput (req/s)

| scenario | conns | AOLserver (C) | aoloxide | aoloxide ÷ C | axum | axum ÷ aoloxide | axum-cached | axum-cached ÷ aoloxide |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| hello | 16 | 25,725 | 33,641 | 1.31× | 158,650 | 4.72× | — | — |
| hello | 64 | 27,463 | 35,591 | 1.30× | 181,911 | 5.11× | — | — |
| hello | 256 | 29,007 | 34,593 | 1.19× | 196,685 | 5.69× | — | — |
| static-1k | 16 | 37,387 | 41,839 | 1.12× | 17,102 | 0.41× | 156,027 | 3.73× |
| static-1k | 64 | 39,801 | 51,473 | 1.29× | 16,373 | 0.32× | 166,198 | 3.23× |
| static-1k | 256 | 48,108 | 50,485 | 1.05× | 15,913 | 0.32× | 177,900 | 3.52× |
| static-64k | 16 | 34,190 | 41,587 | 1.22× | 14,465 | 0.35× | 101,531 | 2.44× |
| static-64k | 64 | 36,544 | 43,704 | 1.20× | 14,518 | 0.33× | 102,536 | 2.35× |
| static-64k | 256 | 40,498 | 44,430 | 1.10× | 13,152 | 0.30× | 106,179 | 2.39× |
| adp-page | 16 | 19,783 | 19,540 | 0.99× | 148,918 | 7.62× | — | — |
| adp-page | 64 | 21,958 | 20,964 | 0.95× | 163,627 | 7.81× | — | — |
| adp-page | 256 | 20,622 | 21,085 | 1.02× | 175,272 | 8.31× | — | — |

## Latency, server CPU and context switches per request, errors

| scenario | conns | server | p50 | p90 | p99 | max | CPU µs/req | ctxsw/req | errors |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|
| hello | 16 | aoloxide | 463 µs | 697 µs | 1.29 ms | 15.30 ms | 59.2 | 1.41 | 0 |
| hello | 16 | aolserver | 553 µs | 853 µs | 3.88 ms | 30.04 ms | 61.8 | 2.24 | 0 |
| hello | 16 | axum | 90 µs | 158 µs | 604 µs | 16.09 ms | 11.8 | 0.04 | 0 |
| hello | 64 | aoloxide | 1.77 ms | 2.79 ms | 4.30 ms | 19.88 ms | 55.4 | 1.19 | 0 |
| hello | 64 | aolserver | 2.11 ms | 3.29 ms | 6.21 ms | 415.26 ms | 59.6 | 1.66 | 0 |
| hello | 64 | axum | 334 µs | 511 µs | 871 µs | 5.21 ms | 10.5 | 0.01 | 0 |
| hello | 256 | aoloxide | 6.77 ms | 12.14 ms | 20.58 ms | 85.37 ms | 57.2 | 0.96 | 0 |
| hello | 256 | aolserver | 7.18 ms | 8.85 ms | 12.83 ms | 225.19 ms | 64.7 | 0.70 | 10 |
| hello | 256 | axum | 1.24 ms | 1.93 ms | 3.25 ms | 14.64 ms | 10.0 | 0.00 | 0 |
| static-1k | 16 | aoloxide | 351 µs | 620 µs | 1.39 ms | 31.19 ms | 47.2 | 1.49 | 0 |
| static-1k | 16 | aolserver | 412 µs | 576 µs | 5.52 ms | 26.22 ms | 41.9 | 1.74 | 0 |
| static-1k | 16 | axum | 893 µs | 1.39 ms | 2.22 ms | 8.85 ms | 106.4 | 9.30 | 0 |
| static-1k | 16 | axum-cached | 96 µs | 148 µs | 309 µs | 6.83 ms | 12.4 | 0.03 | 0 |
| static-1k | 64 | aoloxide | 1.22 ms | 1.92 ms | 2.92 ms | 16.64 ms | 38.7 | 1.31 | 0 |
| static-1k | 64 | aolserver | 1.52 ms | 2.21 ms | 5.29 ms | 209.75 ms | 39.6 | 1.47 | 0 |
| static-1k | 64 | axum | 3.78 ms | 5.81 ms | 8.16 ms | 17.77 ms | 110.2 | 9.33 | 0 |
| static-1k | 64 | axum-cached | 368 µs | 557 µs | 1.29 ms | 24.77 ms | 11.6 | 0.01 | 0 |
| static-1k | 256 | aoloxide | 4.85 ms | 7.92 ms | 12.94 ms | 47.99 ms | 39.0 | 1.24 | 0 |
| static-1k | 256 | aolserver | 4.56 ms | 6.04 ms | 9.13 ms | 219.03 ms | 38.1 | 0.75 | 8 |
| static-1k | 256 | axum | 15.84 ms | 22.95 ms | 30.96 ms | 57.03 ms | 114.6 | 9.74 | 0 |
| static-1k | 256 | axum-cached | 1.41 ms | 2.06 ms | 3.14 ms | 14.65 ms | 11.1 | 0.00 | 0 |
| static-64k | 16 | aoloxide | 365 µs | 567 µs | 1.18 ms | 12.49 ms | 47.6 | 1.44 | 0 |
| static-64k | 16 | aolserver | 437 µs | 626 µs | 5.73 ms | 46.74 ms | 46.5 | 1.77 | 0 |
| static-64k | 16 | axum | 1.07 ms | 1.60 ms | 2.55 ms | 10.37 ms | 129.3 | 9.30 | 0 |
| static-64k | 16 | axum-cached | 139 µs | 219 µs | 410 µs | 8.41 ms | 19.0 | 0.03 | 0 |
| static-64k | 64 | aoloxide | 1.42 ms | 2.27 ms | 3.81 ms | 47.16 ms | 45.0 | 1.21 | 0 |
| static-64k | 64 | aolserver | 1.60 ms | 2.29 ms | 238.59 ms | 1726.57 ms | 43.0 | 1.45 | 0 |
| static-64k | 64 | axum | 4.34 ms | 6.28 ms | 8.59 ms | 23.93 ms | 131.4 | 9.42 | 0 |
| static-64k | 64 | axum-cached | 542 µs | 829 µs | 1.50 ms | 9.48 ms | 18.7 | 0.02 | 0 |
| static-64k | 256 | aoloxide | 5.38 ms | 9.15 ms | 15.79 ms | 64.29 ms | 44.2 | 1.02 | 0 |
| static-64k | 256 | aolserver | 4.30 ms | 5.35 ms | 8.73 ms | 244.24 ms | 44.8 | 0.77 | 0 |
| static-64k | 256 | axum | 19.48 ms | 28.02 ms | 37.28 ms | 55.33 ms | 143.9 | 10.00 | 0 |
| static-64k | 256 | axum-cached | 1.88 ms | 3.04 ms | 4.39 ms | 12.71 ms | 18.0 | 0.01 | 0 |
| adp-page | 16 | aoloxide | 784 µs | 1.21 ms | 3.24 ms | 22.06 ms | 101.0 | 1.23 | 0 |
| adp-page | 16 | aolserver | 722 µs | 1.15 ms | 4.83 ms | 36.21 ms | 83.5 | 2.34 | 0 |
| adp-page | 16 | axum | 100 µs | 158 µs | 364 µs | 7.26 ms | 13.0 | 0.03 | 0 |
| adp-page | 64 | aoloxide | 2.92 ms | 4.63 ms | 9.50 ms | 93.07 ms | 94.9 | 0.67 | 0 |
| adp-page | 64 | aolserver | 2.71 ms | 4.08 ms | 6.42 ms | 214.06 ms | 76.4 | 1.64 | 0 |
| adp-page | 64 | axum | 381 µs | 551 µs | 1.56 ms | 12.84 ms | 11.9 | 0.01 | 0 |
| adp-page | 256 | aoloxide | 11.08 ms | 18.26 ms | 38.55 ms | 92.40 ms | 93.6 | 0.21 | 0 |
| adp-page | 256 | aolserver | 10.93 ms | 13.95 ms | 23.22 ms | 228.67 ms | 89.5 | 0.93 | 8 |
| adp-page | 256 | axum | 1.44 ms | 2.09 ms | 3.06 ms | 14.24 ms | 11.3 | 0.00 | 0 |
